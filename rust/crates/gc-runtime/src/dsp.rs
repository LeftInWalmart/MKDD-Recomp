//! High-level emulation of MKDD's audio DSP microcode (the JAudio / "Zelda-family" ucode).
//!
//! JAudio talks to the DSP through `DSPSendCommands2` (hooked in `hle.rs`):
//! * `0x81` DsetupTable: channel block array (64 × 0x180-byte `JASDsp::TChannel`), filter
//!   tables. Answered immediately through the work callback.
//! * `0x82` DsyncFrame: render N sub-frames of 80 samples into left/right output buffers.
//!   Like the real DSP, each rendered sub-frame is reported with mail `0xF355FF00` and a DSP
//!   interrupt, which runs JAudio's DSP callback (captured from `DspBoot`). That paces the audio
//!   thread exactly like hardware does.
//!
//! Channel mixing implements the parts of the ucode the CPU side relies on: AFC 4-bit/2-bit
//! ADPCM and 8/16-bit PCM from ARAM, pitch (4.12 fixed point), loops with stored predictor
//! history, per-bus mixer volumes (main L/R) or the auto-mixer (volume + pan), and the
//! finished flag. Not emulated: FX/reverb buses, IIR/FIR filters, oscillators, Dolby surround.

use crate::{Ctx, CtxExt, Machine, ARAM_SIZE};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Sub-frames rendered so far (for diagnostics / preflight).
pub static SUBFRAMES_RENDERED: AtomicU64 = AtomicU64::new(0);
/// Channels that had samples mixed in the last rendered sub-frame.
pub static VOICES_LAST: AtomicU64 = AtomicU64::new(0);
/// Most channels mixed in any sub-frame so far.
pub static VOICES_MAX: AtomicU64 = AtomicU64::new(0);
/// Channels seen active (enabled flag set), cumulative.
pub static ACTIVE_SEEN: AtomicU64 = AtomicU64::new(0);

const CH_SIZE: u32 = 0x180;
pub const SUBFRAME: usize = 80;
const BUS_MAIN_L: u16 = 0x0D00;
const BUS_MAIN_R: u16 = 0x0D60;
pub const MAIL_FRAME: u32 = 0xF355_FF00;

#[derive(Clone, Default)]
struct Voice {
    /// Fractional resampling position between `prev` and `cur` input samples.
    frac: f64,
    prev: i32,
    cur: i32,
    /// Decoded AFC block, the index of the next sample to take from it and the end of its valid
    /// samples (`idx >= valid` = need a block).
    samples: [i16; 16],
    idx: usize,
    valid: usize,
    /// Samples to drop from the next decoded block (start / loop points inside a block).
    skip: usize,
    hist: (i32, i32),
}

struct PendingFrame {
    left: u32,
    right: u32,
    subframes: u32,
    next: u32,
    level: f32,
}

#[derive(Default)]
pub struct Dsp {
    /// JAudio's DSP callback (`JASAudioThread::DSPCallback`), from `DspBoot`.
    pub callback: u32,
    ch_buf: u32,
    channels: u32,
    adpcm_table: u32,
    voices: Vec<Voice>,
    frame: Option<PendingFrame>,
    /// DSP → CPU mailbox.
    pub mail: VecDeque<u32>,
}

impl Dsp {
    /// Process a `DSPSendCommands2(cmds, count, cb)` call. Returns true if a frame was queued.
    pub fn command(&mut self, m: &Machine, cmds: u32, count: u32) -> bool {
        let rd = |i: u32| m.read_be(cmds + 4 * i, 4) as u32;
        if count == 0 {
            return false; // release-halt style single mail
        }
        let cmd = rd(0);
        match cmd >> 24 {
            0x81 => {
                self.channels = (cmd & 0xFFFF).min(256);
                self.ch_buf = rd(1);
                self.adpcm_table = rd(3);
                self.voices = vec![Voice { idx: 16, cur: i32::MIN, ..Default::default() }; self.channels as usize];
                crate::diag::log(format!(
                    "DSP: setup {} channels at {:#010x}", self.channels, self.ch_buf
                ));
                false
            }
            0x82 => {
                let subframes = (cmd >> 16) & 0xFF;
                let level = (cmd & 0xFFFF) as f32 / 0x4000 as f32;
                self.frame = Some(PendingFrame { left: rd(1), right: rd(2), subframes, next: 0, level });
                subframes > 0
            }
            op => {
                crate::diag::log(format!("DSP: unhandled command {op:#04x} ({cmd:#010x})"));
                false
            }
        }
    }

    /// Render the next pending sub-frame (if any) and queue its completion mail.
    /// Returns true if more sub-frames remain.
    pub fn step(&mut self, m: &Machine) -> bool {
        let Some(f) = self.frame.as_mut() else { return false };
        if f.next >= f.subframes {
            self.frame = None;
            return false;
        }
        let (k, left, right, level) = (f.next, f.left, f.right, f.level);
        f.next += 1;
        let more = f.next < f.subframes;
        let mut l = [0f32; SUBFRAME];
        let mut r = [0f32; SUBFRAME];
        let mut voices = 0;
        for ch in 0..self.channels {
            voices += self.mix_channel(m, ch, &mut l, &mut r) as u64;
        }
        SUBFRAMES_RENDERED.fetch_add(1, Relaxed);
        VOICES_LAST.store(voices, Relaxed);
        VOICES_MAX.fetch_max(voices, Relaxed);
        let off = k * SUBFRAME as u32 * 2;
        for i in 0..SUBFRAME {
            let cl = (l[i] * level).clamp(-32768.0, 32767.0) as i16;
            let cr = (r[i] * level).clamp(-32768.0, 32767.0) as i16;
            m.write_be(left + off + 2 * i as u32, 2, cl as u16 as u64);
            m.write_be(right + off + 2 * i as u32, 2, cr as u16 as u64);
        }
        self.mail.push_back(MAIL_FRAME);
        if !more {
            self.frame = None;
        }
        more
    }

    /// Mix one channel into the sub-frame; returns true if it produced samples.
    fn mix_channel(&mut self, m: &Machine, ch: u32, out_l: &mut [f32; SUBFRAME], out_r: &mut [f32; SUBFRAME]) -> bool {
        let b = self.ch_buf + ch * CH_SIZE;
        let r16 = |o: u32| m.read_be(b + o, 2) as u16;
        let r32 = |o: u32| m.read_be(b + o, 4) as u32;
        let w16 = |o: u32, v: u16| m.write_be(b + o, 2, v as u64);
        if r16(0x00) == 0 || r16(0x0C) != 0 {
            return false; // inactive or paused
        }
        if ACTIVE_SEEN.fetch_add(1, Relaxed) == 0 {
            crate::diag::log(format!(
                "DSP: first active channel {ch}: pitch {:#x} wave {:#x} blockbytes {} blocksamples {}                  loop {} start {:#x} end {:#x} total {:#x} automix {} vol {:#x} mix0 {:#x}/{:#x} mix1 {:#x}/{:#x}",
                r16(0x04), r32(0x118), r16(0x100), r16(0x64), r16(0x102), r32(0x110), r32(0x114),
                r32(0x11C), r16(0x58), r16(0x56), r16(0x10), r16(0x12), r16(0x18), r16(0x1A)
            ));
        }
        if r16(0x10A) != 0 {
            w16(0x02, 1); // forced stop: report finished
            return false;
        }
        let table = self.adpcm_table;
        let wave = r32(0x118);
        // Sample source type (VPB +0x100), which for ARAM sources is also the number of bytes
        // per 16 samples: 5 / 9 = AFC (2- / 4-bit), 8 = PCM8, 16 = PCM16. Oscillator and
        // main-RAM sources are not emulated.
        let block_bytes = r16(0x100) as u32;
        let (afc, pcm_bytes) = match block_bytes {
            5 | 9 => (true, 0),
            8 => (false, 1),
            16 => (false, 2),
            _ => return false,
        };
        if wave == 0 {
            return false;
        }
        let addr_of = |pos: u32| if afc { wave + pos / 16 * block_bytes } else { wave + pos * pcm_bytes };
        let looped = r16(0x102) != 0;
        let pitch = r16(0x04) as f64 / 4096.0;

        // Volumes: explicit main-bus mixers when routed, else the auto-mixer (volume + pan).
        let (mut vl, mut vr) = (0f32, 0f32);
        for i in 0..4 {
            let mo = 0x10 + 8 * i;
            let target = r16(mo + 2) as i16 as f32 / 32767.0;
            w16(mo + 4, r16(mo + 2)); // current volume reaches target
            match r16(mo) {
                BUS_MAIN_L => vl += target,
                BUS_MAIN_R => vr += target,
                _ => {}
            }
        }
        if vl == 0.0 && vr == 0.0 && r16(0x58) != 0 {
            let vol = r16(0x56) as f32 / 32767.0;
            let pan = ((r16(0x50) >> 8) as f32 / 127.0).clamp(0.0, 1.0);
            vl = vol * (1.0 - pan).sqrt();
            vr = vol * pan.sqrt();
        }

        // Playback state lives in the channel block like on the real DSP, because JAudio reads
        // and rewrites it between sub-frames (JASAramStream steers its ARAM ring buffer through
        // 0x70 = current ARAM address, 0x74 = samples left before the loop/end point,
        // 0x110 = loop position, 0x114 = loop/end position).
        let voice = &mut self.voices[ch as usize];
        let (mut addr, mut remaining);
        if r16(0x08) != 0 {
            let start = r32(0x68);
            *voice = Voice { idx: 16, skip: if afc { (start % 16) as usize } else { 0 }, ..Default::default() };
            addr = addr_of(start);
            remaining = r32(0x114).wrapping_sub(start);
            w16(0x08, 0);
            voice.cur = i32::MIN; // prime on first use
        } else {
            addr = r32(0x70);
            remaining = r32(0x74);
        }
        let aram = m.aram();
        let coef = |idx: usize| -> (i32, i32) {
            let a = table + 4 * idx as u32;
            (m.read_be(a, 2) as u16 as i16 as i32, m.read_be(a + 2, 2) as u16 as i16 as i32)
        };
        let loop_hist = (r16(0x104) as i16 as i32, r16(0x106) as i16 as i32);
        let (loop_pos, loop_end) = (r32(0x110), r32(0x114));
        let mut finished = false;
        // Next input sample, advancing the ARAM address / remaining count like the ucode
        // (semantics as in Dolphin's ZeldaAudioRenderer, which JAudio's ucode shares):
        // * AFC: `remaining` counts samples *decoded* (a block at a time), not played;
        // * at the end of a looping voice the source restarts at the loop block with the loop
        //   history, and `remaining = end - cached - loop` in wrapping u32 arithmetic. A loop
        //   point at or past the end (JASAramStream shrinks/grows its ring buffer around it)
        //   therefore keeps playing; it is never a reason to end the voice.
        let fetch = |voice: &mut Voice, addr: &mut u32, remaining: &mut u32| -> Option<i32> {
            if afc {
                if voice.idx >= voice.valid {
                    if *remaining == 0 {
                        if !looped {
                            return None;
                        }
                        *addr = wave.wrapping_add((loop_pos >> 4) * block_bytes);
                        voice.hist = loop_hist;
                        voice.samples = decode_afc(aram, *addr as usize, block_bytes, &mut voice.hist, &coef);
                        *addr = addr.wrapping_add(block_bytes);
                        let cached = 16 - (loop_pos & 0xF);
                        voice.idx = (loop_pos & 0xF) as usize;
                        voice.valid = 16;
                        voice.skip = 0;
                        *remaining = loop_end.wrapping_sub(cached).wrapping_sub(loop_pos);
                    } else {
                        voice.samples = decode_afc(aram, *addr as usize, block_bytes, &mut voice.hist, &coef);
                        *addr = addr.wrapping_add(block_bytes);
                        let start = voice.skip.min(15);
                        voice.skip = 0;
                        let n = (16 - start as u32).min(*remaining);
                        voice.idx = start;
                        voice.valid = start + n as usize;
                        *remaining -= n;
                    }
                }
                let s = voice.samples[voice.idx] as i32;
                voice.idx += 1;
                Some(s)
            } else {
                if *remaining == 0 {
                    if !looped {
                        return None;
                    }
                    *addr = addr_of(loop_pos);
                    *remaining = loop_end.wrapping_sub(loop_pos);
                }
                *remaining -= 1;
                let a = *addr as usize;
                *addr = addr.wrapping_add(pcm_bytes);
                Some(if pcm_bytes == 1 {
                    if a < ARAM_SIZE { (aram[a] as i8 as i32) << 8 } else { 0 }
                } else if a + 1 < ARAM_SIZE {
                    i16::from_be_bytes([aram[a], aram[a + 1]]) as i32
                } else {
                    0
                })
            }
        };
        if voice.cur == i32::MIN {
            match fetch(voice, &mut addr, &mut remaining) {
                Some(s) => {
                    voice.prev = s;
                    voice.cur = s;
                }
                None => finished = true,
            }
        }
        if !finished {
            for i in 0..SUBFRAME {
                let s = voice.prev as f64 + (voice.cur - voice.prev) as f64 * voice.frac;
                out_l[i] += s as f32 * vl;
                out_r[i] += s as f32 * vr;
                voice.frac += pitch;
                while voice.frac >= 1.0 {
                    voice.frac -= 1.0;
                    match fetch(voice, &mut addr, &mut remaining) {
                        Some(n) => {
                            voice.prev = voice.cur;
                            voice.cur = n;
                        }
                        None => {
                            finished = true;
                            break;
                        }
                    }
                }
                if finished {
                    break;
                }
            }
        }
        m.write_be(b + 0x70, 4, addr as u64);
        m.write_be(b + 0x74, 4, remaining as u64);
        // VPB +0x64: decoded AFC samples not yet consumed. JASAramStream computes the play
        // position as remaining (+0x74) + this, so it must be the real count.
        w16(0x64, if afc { voice.valid.saturating_sub(voice.idx) as u16 } else { 0 });
        if finished {
            w16(0x02, 1);
            if looped {
                // A looping voice should never run out; log it (music stream diagnostics).
                crate::diag::log(format!(
                    "stream: DSP looping voice {ch} finished: pos {addr:#x} remain {remaining:#x} loopstart {loop_pos:#x} end {loop_end:#x} wave {wave:#x}"
                ));
            }
        }
        true
    }
}

/// Decode one AFC block (9 bytes = 4-bit, 5 bytes = 2-bit) into 16 samples.
fn decode_afc(aram: &[u8], a: usize, bytes: u32, hist: &mut (i32, i32), coef: impl Fn(usize) -> (i32, i32)) -> [i16; 16] {
    let mut out = [0i16; 16];
    if a + bytes as usize > aram.len() {
        return out;
    }
    let header = aram[a];
    let delta = 1i32 << ((header >> 4) & 0xF);
    let (c0, c1) = coef((header & 0xF) as usize);
    for (i, o) in out.iter_mut().enumerate() {
        let (val, shift) = if bytes == 9 {
            let byte = aram[a + 1 + i / 2];
            let nib = if i & 1 == 0 { byte >> 4 } else { byte & 15 };
            (((nib as i32) << 28) >> 28, 11)
        } else {
            let byte = aram[a + 1 + i / 4];
            let v = (byte >> (6 - 2 * (i % 4))) & 3;
            (((v as i32) << 30) >> 30, 13)
        };
        let mut s = (delta * val) << shift;
        s += hist.0 * c0 + hist.1 * c1;
        s >>= 11;
        let s = s.clamp(-32768, 32767);
        hist.1 = hist.0;
        hist.0 = s;
        *o = s as i16;
    }
    out
}

/// DSP interrupt: render the next sub-frame and run JAudio's DSP callback, which reads the mail.
pub fn interrupt(c: &mut Ctx) {
    let m = c.m();
    let dsp = &mut m.hw().dsp;
    let more = dsp.step(m);
    let cb = dsp.callback;
    if cb != 0 && !dsp.mail.is_empty() {
        c.call_guest(cb, &[0]);
    }
    if more {
        crate::sched::raise(crate::sched::IRQ_DSP_DSP);
    }
}
