//! Function-level high-level emulation.
//!
//! Every generated function asks `gc_core::hooked` on entry; functions listed in
//! `sheets/hle.csv` (installed at boot via `gc_core::install_hooks`) are routed to [`call`]. Handlers are matched by the
//! function's name from `sheets/functions.csv`, can run the original recompiled body with
//! [`Ctx::call_orig`], and set r3/r4 as the return value. Returning `false` falls through to
//! the original code. Adding an HLE function only needs this file + the sheet, not a rebuild of
//! the generated crates.

/// Stream sound objects (JAIStream) seen being constructed, and the last logged state of each
/// JASAramStream, for the stream lifecycle log.
static STREAM_SOUNDS: std::sync::Mutex<std::collections::BTreeSet<u32>> = std::sync::Mutex::new(std::collections::BTreeSet::new());
static STREAM_STATE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<u32, [u32; 5]>>> =
    std::sync::LazyLock::new(Default::default);

static STREAM_HISTORY: std::sync::LazyLock<std::sync::Mutex<(std::collections::VecDeque<String>, std::collections::HashMap<u32, u32>)>> =
    std::sync::LazyLock::new(Default::default);

fn name_is(m: &crate::Machine, addr: u32, name: &str) -> bool {
    m.name(addr) == name
}

/// The guest call chain: LR, then the saved LRs up the stack frames (for diagnostics).
fn guest_chain(c: &Ctx) -> String {
    let m = c.m();
    let mut out = format!("lr {:#010x} chain", c.r(0x1020, 4) as u32);
    let mut fp = c.gpr(1);
    for _ in 0..8 {
        let next = m.read_be(fp, 4) as u32;
        if !(0x8000_0000..0x8180_0000).contains(&next) || next <= fp {
            break;
        }
        let lr = m.read_be(next + 4, 4) as u32;
        out += &format!(" <- {lr:#010x}");
        fp = next;
    }
    out
}

/// Probe counters for the watchdog's pacing line (audio streaming health).
pub static DVD_READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static ARQ_REQUESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PADREADS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
use crate::{Ctx, CtxExt};

/// `MKDD_PAD_SCRIPT=600:0,4:1000,60:0,...`: scripted port-0 buttons for automated runs. Each
/// `reads:hexmask` segment holds the mask for that many PADRead calls (one per frame); after the
/// last segment the script releases everything. Segment changes are logged with the frame count.
fn pad_script(n: u32) -> Option<u16> {
    static SCRIPT: std::sync::OnceLock<Vec<(u32, u16)>> = std::sync::OnceLock::new();
    let segs = SCRIPT.get_or_init(|| {
        std::env::var("MKDD_PAD_SCRIPT")
            .unwrap_or_default()
            .split(',')
            .filter_map(|seg| {
                let (k, m) = seg.trim().split_once(':')?;
                Some((k.parse().ok()?, u16::from_str_radix(m, 16).ok()?))
            })
            .collect()
    });
    if segs.is_empty() {
        return None;
    }
    let mut start = 0;
    for (i, &(len, mask)) in segs.iter().enumerate() {
        if n < start + len {
            if n == start {
                crate::diag::log(format!("pad script: frame {n} segment {i} mask {mask:#06x}"));
            }
            return Some(mask);
        }
        start += len;
    }
    Some(0)
}

/// The CPU's HID2 (bit 32 set once some thread has published it).
static HID2: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const DVD_STATE_END: u64 = 0;
const DVD_COMMAND_READ: u64 = 1;

/// The hottest hooks (hundreds of thousands of calls per second in 3D scenes), resolved by
/// address once so they skip the name lookup and the string match below.
#[derive(Clone, Copy)]
enum Hot {
    Disable,
    Enable,
    Restore,
    Mtx(&'static str),
}

fn hot(addr: u32) -> Option<Hot> {
    static T: std::sync::OnceLock<Vec<(u32, Hot)>> = std::sync::OnceLock::new();
    let t = T.get_or_init(|| {
        let m = crate::machine();
        let mut v: Vec<(u32, Hot)> = [
            ("OSDisableInterrupts", Hot::Disable),
            ("OSEnableInterrupts", Hot::Enable),
            ("OSRestoreInterrupts", Hot::Restore),
            ("PSMTXCopy", Hot::Mtx("PSMTXCopy")),
            ("PSMTXConcat", Hot::Mtx("PSMTXConcat")),
            ("PSMTXMultVec", Hot::Mtx("PSMTXMultVec")),
            ("PSVECCrossProduct", Hot::Mtx("PSVECCrossProduct")),
            ("PSVECMag", Hot::Mtx("PSVECMag")),
            ("PSMTXInverse", Hot::Mtx("PSMTXInverse")),
        ]
        .iter()
        .filter_map(|(n, h)| (m.addr_of)(n).map(|a| (a, *h)))
        .collect();
        v.sort_by_key(|e| e.0);
        v
    });
    t.binary_search_by_key(&addr, |e| e.0).ok().map(|i| t[i].1)
}

pub fn call(c: &mut Ctx, addr: u32) -> bool {
    if let Some(h) = hot(addr) {
        let msr = c.r(gc_core::REG_MSR, 4);
        let was = ((msr & gc_core::MSR_EE) != 0) as u32;
        match h {
            Hot::Disable => {
                c.wr(gc_core::REG_MSR, 4, msr & !gc_core::MSR_EE);
                c.set_gpr(3, was);
            }
            Hot::Enable => {
                c.wr(gc_core::REG_MSR, 4, msr | gc_core::MSR_EE);
                c.set_gpr(3, was);
            }
            Hot::Restore => {
                let new = if c.gpr(3) != 0 { msr | gc_core::MSR_EE } else { msr & !gc_core::MSR_EE };
                c.wr(gc_core::REG_MSR, 4, new);
                c.set_gpr(3, was);
            }
            Hot::Mtx(name) => {
                crate::psmtx::call(c, name);
            }
        }
        return true;
    }
    let m = c.m();
    let s = &m.sched;
    match m.name(addr) {
        // ---- threads ----
        "OSCreateThread" => {
            let t = c.gpr(3);
            c.call_orig(addr); // fills the OSThread/OSContext; returns TRUE in r3
            s.create(c, t);
        }
        "OSResumeThread" => {
            crate::diag::log(format!("OSResumeThread {:#x} from {:#x} (lr {:#x})", c.gpr(3), c.thread, c.r(0x1020, 4) as u32));
            let prev = s.resume(c, c.gpr(3));
            c.set_gpr(3, prev as u32);
        }
        "OSSuspendThread" => {
            crate::diag::log(format!("OSSuspendThread {:#x} from {:#x} (lr {:#x})", c.gpr(3), c.thread, c.r(0x1020, 4) as u32));
            let prev = s.suspend(c, c.gpr(3));
            c.set_gpr(3, prev as u32);
        }
        "OSSleepThread" => s.sleep(c, c.gpr(3)),
        "OSWakeupThread" => s.wakeup(c, c.gpr(3)),
        "OSYieldThread" => s.yield_now(c),
        "__OSReschedule" => s.reschedule(c),
        "OSExitThread" => {
            let t = c.thread;
            m.write_be(t + 0x2D8, 4, c.gpr(3) as u64);
            s.exit(c);
            park_forever();
        }
        "OSCancelThread" => {
            if c.gpr(3) == c.thread {
                s.exit(c);
                park_forever();
            }
            s.cancel(c, c.gpr(3));
        }
        "SelectThread" | "OSLoadContext" => {
            crate::diag::log(format!("{} reached (unexpected with HLE scheduler)", m.name(addr)));
        }

        // ---- alarms ----
        "OSSetAlarm" => {
            let (alarm, handler) = (c.gpr(3), c.gpr(7));
            let fire = crate::sched::timebase() + arg64(c, 5);
            m.write_be(alarm, 4, handler as u64);
            m.write_be(alarm + 0x08, 8, fire);
            m.write_be(alarm + 0x18, 8, 0);
            s.set_alarm(alarm, handler, fire, 0);
        }
        "OSSetPeriodicAlarm" => {
            let (alarm, handler) = (c.gpr(3), c.gpr(9));
            let (start, period) = (arg64(c, 5), arg64(c, 7).max(1));
            let now = crate::sched::timebase();
            let fire = if start > now { start } else { start + ((now - start) / period + 1) * period };
            m.write_be(alarm, 4, handler as u64);
            m.write_be(alarm + 0x08, 8, fire);
            m.write_be(alarm + 0x18, 8, period);
            m.write_be(alarm + 0x20, 8, start);
            s.set_alarm(alarm, handler, fire, period);
        }
        "OSCancelAlarm" => {
            s.cancel_alarm(c.gpr(3));
            m.write_be(c.gpr(3), 4, 0);
        }

        // ---- DVD (reads go straight to the disc image) ----
        "DVDInit" => {
            if let Some(a) = (m.addr_of)("__DVDFSInit") {
                c.call(a as u64);
            }
        }
        "DVDReadAbsAsyncPrio" => {
            let (block, dst, len, off, cb) = (c.gpr(3), c.gpr(4), c.gpr(5), c.gpr(6), c.gpr(7));
            DVD_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            dvd_read(c, off as u64, dst, len);
            for (o, v) in [(0x08, DVD_COMMAND_READ), (0x0C, DVD_STATE_END), (0x10, off as u64),
                           (0x14, len as u64), (0x18, dst as u64), (0x1C, len as u64),
                           (0x20, len as u64), (0x28, cb as u64)] {
                m.write_be(block + o, 4, v);
            }
            if cb != 0 {
                c.call_guest(cb, &[len, block]);
            }
            c.set_gpr(3, 1);
        }
        "DVDReadDiskID" => {
            let (block, dst, cb) = (c.gpr(3), c.gpr(4), c.gpr(5));
            dvd_read(c, 0, dst, 0x20);
            m.write_be(block + 0x0C, 4, DVD_STATE_END);
            if cb != 0 {
                c.call_guest(cb, &[0x20, block]);
            }
            c.set_gpr(3, 1);
        }
        "DVDInquiryAsync" => {
            let (block, info, cb) = (c.gpr(3), c.gpr(4), c.gpr(5));
            for i in 0..8 {
                m.write_be(info + 4 * i, 4, 0);
            }
            m.write_be(block + 0x0C, 4, DVD_STATE_END);
            if cb != 0 {
                c.call_guest(cb, &[0x20, block]);
            }
            c.set_gpr(3, 1);
        }
        "DVDCancelAsync" => {
            let (block, cb) = (c.gpr(3), c.gpr(4));
            m.write_be(block + 0x0C, 4, DVD_STATE_END);
            if cb != 0 {
                c.call_guest(cb, &[0, block]);
            }
            c.set_gpr(3, 1);
        }
        "DVDCancel" | "DVDGetDriveStatus" | "DVDReset" | "DVDResume" => c.set_gpr(3, 0),
        "DVDCheckDisk" => c.set_gpr(3, 1),
        "DVDGetCommandBlockStatus" => {
            let st = m.read_be(c.gpr(3) + 0x0C, 4) as u32;
            c.set_gpr(3, st);
        }

        // ---- controllers ----
        "PADInit" | "PADReset" | "PADRecalibrate" => c.set_gpr(3, 1),
        "PADControlMotor" => {}
        "PADSetSamplingCallback" => c.set_gpr(3, 0),
        "PADRead" => {
            let dst = c.gpr(3);
            let mut ports = *m.pad.ports.lock().unwrap();
            if let Ok(v) = std::env::var("MKDD_FAKE_PAD") {
                // Diagnostic: hold a button mask on port 0 (hex), e.g. 1000 = Start.
                let n = PADREADS.load(std::sync::atomic::Ordering::Relaxed);
                if (n / 30) % 2 == 1 {
                    ports[0].button = u16::from_str_radix(&v, 16).unwrap_or(0);
                }
                ports[0].connected = true;
            }
            let n = PADREADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(mask) = pad_script(n) {
                ports[0].button = mask;
                ports[0].connected = true;
            }
            for (i, p) in ports.iter().enumerate() {
                for (j, b) in p.to_bytes().iter().enumerate() {
                    m.write_be(dst + 12 * i as u32 + j as u32, 1, *b as u64);
                }
            }
            c.set_gpr(3, 0);
        }

        "SIProbe" | "SIGetType" => {
            // SI_GC_CONTROLLER for connected ports, SI_ERROR_NO_RESPONSE otherwise.
            let ch = (c.gpr(3) & 3) as usize;
            let connected = m.pad.ports.lock().unwrap()[ch].connected
                || ch == 0 && (std::env::var_os("MKDD_FAKE_PAD").is_some() || std::env::var_os("MKDD_PAD_SCRIPT").is_some());
            c.set_gpr(3, if connected { 0x0900_0000 } else { 0x08 });
        }

        // ---- music stream lifecycle log (diagnostic; enable by listing these functions in
        // sheets/hle.csv: JAIStream ctor, JAISound::stop, JAIStream tryDie, JASAramStream
        // start/stop/pause/cancel, updateChannel) ----
        "__ct__9JAIStreamFP12JAIStreamMgrP31JAISoundStrategyMgr<9JAIStream>" => {
            let this = c.gpr(3);
            c.call_orig(addr);
            STREAM_SOUNDS.lock().unwrap().insert(this);
        }
        "stop__8JAISoundFUl" | "stop__8JAISoundFv" => {
            let this = c.gpr(3);
            if STREAM_SOUNDS.lock().unwrap().contains(&this) {
                let fade = if name_is(m, addr, "stop__8JAISoundFUl") { c.gpr(4) as i64 } else { -1 };
                crate::diag::log(format!("stream: JAISound::stop sound {this:#x} fade {fade} {}", guest_chain(c)));
            }
            c.call_orig(addr);
        }
        "JAISound_tryDie___9JAIStreamFv" | "FUN_800999e4" | "FUN_80099a18" | "FUN_80099a54" | "FUN_80099aa4" => {
            let what = match m.name(addr) {
                "FUN_800999e4" => "JASAramStream::start",
                "FUN_80099a18" => "JASAramStream::stop",
                "FUN_80099a54" => "JASAramStream::pause",
                "FUN_80099aa4" => "JASAramStream::cancel",
                _ => "JAIStream::tryDie",
            };
            crate::diag::log(format!("stream: {what} obj {:#x} arg {:#x} {}", c.gpr(3), c.gpr(4), guest_chain(c)));
            c.call_orig(addr);
        }
        "updateChannel__13JASAramStreamFUlP10JASChannelPQ26JASDsp8TChannel" => {
            let (st, kind, dsp) = (c.gpr(3), c.gpr(4), c.gpr(6));
            c.call_orig(addr);
            // Stream status (+0xAE), pending loads (+0x118), loop count (+0xC4), played samples
            // (+0xB8) vs total (+0x170), static error / DVD-error flags (r13-0x64F7 / -0x64F8).
            let r13 = c.gpr(13);
            let state = [
                m.read_be(st + 0xAE, 1) as u32,
                m.read_be(st + 0x118, 4) as u32,
                m.read_be(r13.wrapping_sub(0x64F7), 1) as u32,
                m.read_be(r13.wrapping_sub(0x64F8), 1) as u32,
                m.read_be(st + 0xC4, 4) as u32,
            ];
            // Short history of raw stream / voice state; dumped when the played-sample counter
            // jumps by a ring length or more between callbacks.
            {
                let rec = format!(
                    "cb {kind} dsp {dsp:#x} pos {:#x} remain {:#x} cached {} | prev {:#x} played {:#x} blk {:#x} rd {:#x} ad {} c0 {} ring {}/{} upd {:#x} loopstart {:#x} end {:#x}",
                    m.read_be(dsp + 0x70, 4), m.read_be(dsp + 0x74, 4), m.read_be(dsp + 0x64, 2),
                    m.read_be(st + 0xB4, 4), m.read_be(st + 0xB8, 4), m.read_be(st + 0xBC, 4), m.read_be(st + 0xB0, 4),
                    m.read_be(st + 0xAD, 1), m.read_be(st + 0xC0, 1), m.read_be(st + 0x15C, 4), m.read_be(st + 0x160, 4),
                    m.read_be(st + 0x12C, 4), m.read_be(dsp + 0x110, 4), m.read_be(dsp + 0x114, 4),
                );
                let played = m.read_be(st + 0xB8, 4) as u32;
                let mut h = STREAM_HISTORY.lock().unwrap();
                let jump = h.1.get(&st).is_some_and(|p| played.wrapping_sub(*p) > 0x10000 && played > *p);
                h.1.insert(st, played);
                h.0.push_back(rec);
                if h.0.len() > 24 {
                    h.0.pop_front();
                }
                if jump {
                    let dump: Vec<String> = h.0.iter().cloned().collect();
                    crate::diag::log(format!("stream: played jump on {st:#x}; last callbacks:
    {}", dump.join("
    ")));
                }
            }
            let mut last = STREAM_STATE.lock().unwrap();
            if kind >= 2 || last.get(&st) != Some(&state) {
                crate::diag::log(format!(
                    "stream: updateChannel {st:#x} cb {kind} status {:#x} pending {} fatal {} dvderr {} loops {} played {:#x}/{:#x} \
                     | dsp {dsp:#x} done {} pause {} loop {} pos {:#x} remain {:#x} loopstart {:#x} end {:#x}",
                    state[0], state[1], state[2], state[3], state[4] as i32,
                    m.read_be(st + 0xB8, 4), m.read_be(st + 0x170, 4),
                    m.read_be(dsp + 0x02, 2), m.read_be(dsp + 0x0C, 2), m.read_be(dsp + 0x102, 2),
                    m.read_be(dsp + 0x70, 4), m.read_be(dsp + 0x74, 4), m.read_be(dsp + 0x110, 4), m.read_be(dsp + 0x114, 4),
                ));
                last.insert(st, state);
            }
        }

        // ---- music stream diagnostics: every path that sets JASAramStream's fatal flag ----
        "sendCmdMsg__13JASTaskThreadFPFPv_vPv" | "sendCmdMsg__13JASTaskThreadFPFPv_vPCvUl" | "DVDReadPrio" | "FUN_8000a7d8" => {
            let name = m.name(addr);
            let args = [c.gpr(3), c.gpr(4), c.gpr(5), c.gpr(6)];
            let lr = c.r(0x1020, 4) as u32;
            c.call_orig(addr);
            let r = c.gpr(3);
            let failed = if name == "DVDReadPrio" { (r as i32) < 0 } else { r == 0 };
            if failed {
                let mut extra = String::new();
                if name.starts_with("sendCmdMsg") {
                    // JASTaskThread: OSMessageQueue at +0x30 (msgCount +0x14, usedCount +0x1C),
                    // message pool at +0x18 (bytes used +4, live messages +8).
                    let t = args[0];
                    let pool = m.read_be(t + 0x18, 4) as u32;
                    extra = format!(
                        " queue {}/{} pool {:#x} used {:#x} live {}",
                        m.read_be(t + 0x30 + 0x1C, 4), m.read_be(t + 0x30 + 0x14, 4), pool,
                        if pool != 0 { m.read_be(pool + 4, 4) } else { 0 },
                        if pool != 0 { m.read_be(pool + 8, 4) } else { 0 }
                    );
                }
                crate::diag::log(format!(
                    "stream: {name} failed -> {r:#x} (args {:#x} {:#x} {:#x} {:#x}, lr {lr:#010x}, thread {:#x}){extra}",
                    args[0], args[1], args[2], args[3], c.thread
                ));
            }
        }

        // ---- GX ----
        "__GXFifoLink" => {
            m.hw().gx_link = c.gpr(3) & 0xFF != 0;
            c.call_orig(addr);
        }

        // ---- audio: no DSP microcode emulation yet; skip boot handshakes and calibration ----
        "__AI_SRC_INIT" | "__OSInitAudioSystem" | "__OSStopAudioSystem" => {}
        // ---- DSP (audio microcode HLE in dsp.rs) ----
        "DspBoot__FPFPv_v" => {
            m.hw().dsp.callback = c.gpr(3);
            crate::diag::log(format!("DSP: JAudio callback {:#010x}", c.gpr(3)));
            // The boot handshake would end in Dsp_Running_Start(); without it DsyncFrame2 only
            // queues frames and never sends them.
            if let Some(a) = (m.addr_of)("Dsp_Running_Start__Fv") {
                c.call(a as u64);
            }
        }
        "DSPSendCommands2__FPUlUlPFUs_v" => {
            let (cmds, count, cb) = (c.gpr(3), c.gpr(4), c.gpr(5));
            let frame = m.hw().dsp.command(m, cmds, count);
            if cb != 0 {
                c.call_guest(cb, &[0]);
            }
            if frame {
                crate::sched::raise(crate::sched::IRQ_DSP_DSP);
            }
            c.set_gpr(3, 1);
        }
        // JAudio busy-waits for the DSP task to report it's running; without DSP emulation
        // that never happens and the spinning (high-priority) audio thread starves the game.
        "Dsp_Running_Check__Fv" => c.set_gpr(3, 1),

        // ---- diagnostics / system ----
        // ---- MTX library (native; top of the profile in 3D scenes) ----
        "PSMTXCopy" | "PSMTXConcat" | "PSMTXMultVec" | "PSVECCrossProduct" | "PSVECMag" | "PSMTXInverse" => {
            crate::psmtx::call(c, m.name(addr));
        }
        // ---- MSR.EE ----
        // Ghidra's mtmsr p-code keeps the old EE (and ME) bits, so the recompiled versions of
        // these never change the interrupt-enable state: every OS critical section could be
        // interrupted (the ARAM queue's chunk bookkeeping got corrupted that way).
        "OSDisableInterrupts" => {
            let msr = c.r(gc_core::REG_MSR, 4);
            c.wr(gc_core::REG_MSR, 4, msr & !gc_core::MSR_EE);
            c.set_gpr(3, ((msr & gc_core::MSR_EE) != 0) as u32);
        }
        "OSEnableInterrupts" => {
            let msr = c.r(gc_core::REG_MSR, 4);
            c.wr(gc_core::REG_MSR, 4, msr | gc_core::MSR_EE);
            c.set_gpr(3, ((msr & gc_core::MSR_EE) != 0) as u32);
        }
        "OSRestoreInterrupts" => {
            let msr = c.r(gc_core::REG_MSR, 4);
            let new = if c.gpr(3) != 0 { msr | gc_core::MSR_EE } else { msr & !gc_core::MSR_EE };
            c.wr(gc_core::REG_MSR, 4, new);
            c.set_gpr(3, ((msr & gc_core::MSR_EE) != 0) as u32);
        }
        "PPCMtmsr" => c.wr(gc_core::REG_MSR, 4, c.gpr(3) as u64),
        // HID2 (locked cache enable, paired-single enable, ...) belongs to the CPU, not to a
        // thread, but every host thread has its own register file. The few functions that
        // touch it see the shared copy on entry and publish it on exit.
        "PPCMfhid2" | "PPCMthid2" | "__OSFPRInit" | "__LCEnable" | "LCDisable" | "LCQueueWait"
        | "__OSLoadFPUContext" | "__OSSaveFPUContext" | "OSFillFPUContext" | "FUN_80110a50"
        | "FUN_80110c08" => {
            const REG_HID2: usize = 0x1E60;
            let g = HID2.load(std::sync::atomic::Ordering::SeqCst);
            if g >> 32 != 0 {
                c.wr(REG_HID2, 4, g & 0xFFFF_FFFF);
            }
            c.call_orig(addr);
            HID2.store(1 << 32 | c.r(REG_HID2, 4), std::sync::atomic::Ordering::SeqCst);
        }
        "FUN_80012df0" => {
            // Diagnostic (MKDD_HEAP_CHECK=1): JKRExpHeap free list = heap+0x78, block: +4 size,
            // +8 prev, +0xC next.
            if !diag_enabled(&HEAP_CHECK, "MKDD_HEAP_CHECK") {
                return false;
            }
            let heap = c.gpr(3);
            let mut b = m.read_be(heap + 0x78, 4) as u32;
            let mut prev = 0u32;
            for _ in 0..100_000 {
                if b == 0 {
                    break;
                }
                if !(0x8000_0000..0x8180_0000).contains(&b) {
                    let words: Vec<String> = (0..8).map(|k| format!("{:08x}", m.read_be(prev + 4 * k, 4))).collect();
                    crate::diag::log(format!(
                        "HEAP CORRUPT heap {heap:#x}: block {prev:#x} has next {b:#x}; header {}",
                        words.join(" ")
                    ));
                    break;
                }
                prev = b;
                b = m.read_be(b + 0xC, 4) as u32;
            }
            return false;
        }
        "__ARQServiceQueueLo" | "__ARQInterruptServiceRoutine" => {
            if diag_enabled(&ARQ_TRACE, "MKDD_ARQ_TRACE") {
                arq_event(c, m.name(addr));
            }
            return false;
        }
        "ARQPostRequest" => {
            ARQ_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if !diag_enabled(&ARQ_TRACE, "MKDD_ARQ_TRACE") {
                return false;
            }
            arq_event(c, "ARQPostRequest");
            // ARQPostRequest(req, owner, type, prio, src, dst, len, cb): remember the request.
            let rec = ArqRec {
                ty: c.gpr(5), prio: c.gpr(6), src: c.gpr(7), dst: c.gpr(8), len: c.gpr(9),
                cb: c.gpr(10), lr: c.r(0x1020, 4) as u32, thread: c.thread,
            };
            arq_posts().lock().unwrap().insert(c.gpr(3), rec);
            return false;
        }
        "ARStartDMA" => {
            if !diag_enabled(&ARQ_TRACE, "MKDD_ARQ_TRACE") {
                return false;
            }
            arq_event(c, "ARStartDMA");
            // ARStartDMA(type, mm, aram, len): is it the in-flight ARQ request, unmodified?
            let (ty, mm, ar, len) = (c.gpr(3), c.gpr(4), c.gpr(5), c.gpr(6));
            let r13 = c.gpr(13);
            let lo = m.read_be(r13.wrapping_sub(0x62fc), 4) as u32;
            let hi = m.read_be(r13.wrapping_sub(0x6300), 4) as u32;
            let (main_lo, aram_lo) = if ty == 0 { (mm, ar) } else { (mm, ar) };
            let posts = arq_posts().lock().unwrap();
            let known = |req: u32| {
                posts.get(&req).is_some_and(|p| {
                    let (pm, pa) = if p.ty == 0 { (p.src, p.dst) } else { (p.dst, p.src) };
                    p.ty == ty && main_lo >= pm && main_lo + len <= pm + p.len && aram_lo >= pa && aram_lo + len <= pa + p.len
                })
            };
            if !(known(lo) || known(hi) || posts.values().any(|p| {
                let (pm, pa) = if p.ty == 0 { (p.src, p.dst) } else { (p.dst, p.src) };
                p.ty == ty && main_lo >= pm && main_lo + len <= pm + p.len && aram_lo >= pa && aram_lo + len <= pa + p.len
            })) {
                let desc = |req: u32| posts.get(&req).map_or("never posted".to_string(), |p| format!(
                    "posted type {} prio {} src {:#x} dst {:#x} len {:#x} cb {:#x} by lr {:#x} on {:#x}",
                    p.ty, p.prio, p.src, p.dst, p.len, p.cb, p.lr, p.thread));
                let cur = |req: u32| if req == 0 { String::new() } else { format!(
                    " now [type {:#x} src {:#x} dst {:#x} len {:#x} cb {:#x}]",
                    m.read_be(req + 8, 4), m.read_be(req + 0x10, 4), m.read_be(req + 0x14, 4),
                    m.read_be(req + 0x18, 4), m.read_be(req + 0x1c, 4)) };
                if !ARQ_DUMPED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    for e in arq_ring().lock().unwrap().iter() {
                        crate::diag::log(format!("ARQ event: {e}"));
                    }
                }
                crate::diag::log(format!(
                    "ARQ MISMATCH: DMA type {ty} mm {mm:#x} aram {ar:#x} len {len:#x} (lr {:#x}); lo req {lo:#x}: {}{}; hi req {hi:#x}: {}{}",
                    c.r(0x1020, 4), desc(lo), cur(lo), desc(hi), cur(hi)
                ));
            }
            return false;
        }
        "THPVideoDecode" => {
            // THPVideoDecode(file, tileY, tileU, tileV, work): decode natively (the recompiled
            // SDK decoder runs at under half real time); fall back to it if the stream is unusual.
            static NATIVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let native = *NATIVE.get_or_init(|| std::env::var("MKDD_THP_NATIVE").map_or(true, |v| v != "0"));
            let (file, dst) = (c.gpr(3), [c.gpr(4), c.gpr(5), c.gpr(6)]);
            if native && file != 0 && crate::thp::decode(m, file, dst) {
                c.set_gpr(3, 0);
                return true;
            }
            if native {
                crate::diag::log(format!("THP: native decode declined frame at {file:#x}; using SDK decoder"));
            }
            c.call_orig(addr);
            if c.gpr(3) != 0 {
                crate::diag::log(format!("THPVideoDecode error {} (file {:#x})", c.gpr(3) as i32, m.read_be(0x803b1834, 4)));
            }
        }
        "OSReport" => {
            let msg = format_guest(c, c.gpr(3), 4);
            crate::diag::log(format!("OSReport: {}", msg.trim_end()));
        }
        "OSPanic" => {
            let file = read_cstr(c, c.gpr(3));
            let line = c.gpr(4);
            let msg = format_guest(c, c.gpr(5), 6);
            crate::diag::log(format!("OSPanic {file}:{line}: {}", msg.trim_end()));
            park_forever();
        }
        "OSResetSystem" | "__OSDoHotReset" => {
            crate::diag::log(format!("{} called (ignored)", m.name(addr)));
        }
        "PPCHalt" => {
            crate::diag::log("PPCHalt".into());
            park_forever();
        }
        // ---- "Unlock all" stays out of the memory card (cheats.rs) ----
        "fetch__10SystemFileFv" => crate::cheats::around_save(|| c.call_orig(addr)),
        "store__10SystemFileFv" => {
            c.call_orig(addr);
            crate::cheats::after_load();
        }
        name if name.starts_with("CARD") => match crate::memcard::call(c, name) {
            Some(r) => {
                crate::diag::log(format!("{name} -> {r}"));
                c.set_gpr(3, r as u32);
            }
            None => return false,
        },
        _ => return false,
    }
    true
}

fn park_forever() -> ! {
    loop {
        std::thread::park();
    }
}

/// 64-bit argument in a register pair (PPC EABI: rN = high word, rN+1 = low word).
fn arg64(c: &Ctx, n: usize) -> u64 {
    ((c.gpr(n) as u64) << 32) | c.gpr(n + 1) as u64
}

fn dvd_read(c: &mut Ctx, off: u64, dst: u32, len: u32) {
    let m = c.m();
    let Some(o) = crate::Machine::ram_off(dst) else { return };
    let len = (len as usize).min(crate::RAM_SIZE - o);
    match &m.disc {
        Some(d) => d.read(off, &mut m.ram()[o..o + len]),
        None => crate::diag::log(format!("DVD read {off:#x}+{len:#x} but no disc image is loaded")),
    }
}

fn read_cstr(c: &Ctx, addr: u32) -> String {
    let mut out = Vec::new();
    for i in 0..1024 {
        let b = c.m().read_be(addr + i, 1) as u8;
        if b == 0 {
            break;
        }
        out.push(b);
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Minimal printf for OSReport/OSPanic: integer, char and string conversions.
fn format_guest(c: &Ctx, fmt_addr: u32, first_arg: usize) -> String {
    let fmt = read_cstr(c, fmt_addr);
    let mut out = String::new();
    let mut arg = first_arg;
    let mut next = |c: &Ctx| {
        let v = if arg <= 10 { c.gpr(arg) } else { 0 };
        arg += 1;
        v
    };
    let mut chars = fmt.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let mut spec = String::new();
        while let Some(&n) = chars.peek() {
            chars.next();
            if n.is_ascii_alphabetic() && !matches!(n, 'l' | 'h') || n == '%' {
                spec.push(n);
                break;
            }
            spec.push(n);
        }
        match spec.chars().last() {
            Some('d') | Some('i') => out += &(next(c) as i32).to_string(),
            Some('u') => out += &next(c).to_string(),
            Some('x') | Some('p') => out += &format!("{:x}", next(c)),
            Some('X') => out += &format!("{:X}", next(c)),
            Some('c') => out.push(next(c) as u8 as char),
            Some('s') => {
                let a = next(c);
                out += &read_cstr(c, a);
            }
            Some('%') => out.push('%'),
            _ => out += &format!("%{spec}"),
        }
    }
    out
}

/// Posted ARAM-queue requests (diagnostic for the post-movie heap corruption).
struct ArqRec {
    ty: u32,
    prio: u32,
    src: u32,
    dst: u32,
    len: u32,
    cb: u32,
    lr: u32,
    thread: u32,
}

fn arq_posts() -> &'static std::sync::Mutex<std::collections::HashMap<u32, ArqRec>> {
    static P: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, ArqRec>>> = std::sync::OnceLock::new();
    P.get_or_init(Default::default)
}

static ARQ_DUMPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn arq_ring() -> &'static std::sync::Mutex<std::collections::VecDeque<String>> {
    static R: std::sync::OnceLock<std::sync::Mutex<std::collections::VecDeque<String>>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

/// Record an ARQ event with the queue state (r13-relative ARQ globals) and context.
fn arq_event(c: &Ctx, what: &str) {
    let m = c.m();
    let r13 = c.gpr(13);
    let g = |o: u32| m.read_be(r13.wrapping_sub(o), 4) as u32;
    let line = format!(
        "{what:<30} thread {:#x} irq {} EE {} lr {:#x} r3..r6 {:#x} {:#x} {:#x} {:#x} | hiQ {:#x} loQ {:#x} hiPend {:#x} loPend {:#x} hiCb {:#x} loCb {:#x} chunk {:#x}",
        c.thread, c.in_irq, (c.r(gc_core::REG_MSR, 4) >> 15) & 1, c.r(0x1020, 4),
        c.gpr(3), c.gpr(4), c.gpr(5), c.gpr(6),
        g(0x6310), g(0x6308), g(0x6300), g(0x62fc), g(0x62f8), g(0x62f4), g(0x62f0)
    );
    let mut r = arq_ring().lock().unwrap();
    if r.len() >= 40 {
        r.pop_front();
    }
    r.push_back(line);
}

static HEAP_CHECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
static ARQ_TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Opt-in diagnostics (they sit on hot paths): enabled when the env var is set and not "0".
fn diag_enabled(flag: &'static std::sync::OnceLock<bool>, var: &str) -> bool {
    *flag.get_or_init(|| std::env::var(var).is_ok_and(|v| v != "0"))
}
