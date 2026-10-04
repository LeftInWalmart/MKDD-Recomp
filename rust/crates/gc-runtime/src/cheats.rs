//! "Unlock all" (P menu): a session-only cheat that never reaches the memory card.
//!
//! The game's save record (`SystemRecord`, 0x803BC1C0) keeps the unlocks as two bit sets, which
//! the game itself sets through `SequenceInfo::setSecretKartAppear` / `setSecretGameAppear`:
//! +4 = secret karts (bits 0-12), +6 = secret game flags (bits 0-8: hidden characters, Special
//! Cup, Mirror mode, All-Cup Tour, ...). Setting every bit is what a 100% save holds.
//!
//! While the cheat is on, the record in RAM has every bit set, and the player's own earned bits
//! are kept aside. The two places the record crosses the memory card are hooked (hle.csv):
//! * `SystemFile::fetch` (record -> card buffer, i.e. saving): the earned bits are put back for
//!   the copy and the unlocks re-applied afterwards, so the card only ever holds real progress;
//! * `SystemFile::store` (card buffer -> record, i.e. loading): the loaded bits become the earned
//!   bits, then the unlocks are re-applied.
//!
//! Nothing is remembered across launches: the button has to be pressed again each session.

use std::sync::Mutex;

const SECRET_KARTS: (u32, u16) = (0x803B_C1C4, 0x1FFF);
const SECRET_GAME: (u32, u16) = (0x803B_C1C6, 0x01FF);

struct Unlock {
    active: bool,
    /// The player's own bits (karts, game flags).
    earned: [u16; 2],
}

static UNLOCK: Mutex<Unlock> = Mutex::new(Unlock { active: false, earned: [0; 2] });

fn read(addr: u32) -> u16 {
    crate::machine().read_be(addr, 2) as u16
}

fn write(addr: u32, v: u16) {
    crate::machine().write_be(addr, 2, v as u64);
}

/// Set every unlock bit. Bits that aren't all set were last written by the game itself, so they
/// are the player's real progress: remember them first.
fn apply(u: &mut Unlock) {
    for (k, (addr, all)) in [SECRET_KARTS, SECRET_GAME].into_iter().enumerate() {
        let v = read(addr);
        if v & all != all {
            u.earned[k] = v;
            write(addr, v | all);
        }
    }
}

/// The P menu's "Unlock all" button.
pub fn unlock_all() {
    let mut u = UNLOCK.lock().unwrap();
    u.active = true;
    apply(&mut u);
}

pub fn unlock_active() -> bool {
    UNLOCK.lock().unwrap().active
}

/// Called every frame: keeps the unlocks set (the game resets the record when a new save file
/// is created).
pub fn tick() {
    let mut u = UNLOCK.lock().unwrap();
    if u.active {
        apply(&mut u);
    }
}

/// `SystemFile::fetch` (saving): run `save` with the player's earned bits in the record.
pub fn around_save(save: impl FnOnce()) {
    let mut u = UNLOCK.lock().unwrap();
    if !u.active {
        drop(u);
        return save();
    }
    write(SECRET_KARTS.0, u.earned[0]);
    write(SECRET_GAME.0, u.earned[1]);
    save();
    apply(&mut u);
}

/// `SystemFile::store` (loading): the record now holds the card's bits; they are the earned
/// ones even when the card already has everything unlocked.
pub fn after_load() {
    let mut u = UNLOCK.lock().unwrap();
    if u.active {
        u.earned = [read(SECRET_KARTS.0), read(SECRET_GAME.0)];
        apply(&mut u);
    }
}
