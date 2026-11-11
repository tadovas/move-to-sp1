//! The `sui::clock::Clock` singleton (object 0x6): `clock::borrow()` returns a reference to
//! it; tests move its time with `clock::set_for_testing` / `test_scenario` helpers.

use core::ptr;

use move_rt_core::{
    desc::{self, Kind, Slot, TypeDesc},
    global::Global,
};

/// (address of the clock object once materialized, current timestamp)
static CLOCK: Global<(*mut u8, u64)> = Global::new((ptr::null_mut(), 0));

const CLOCK_ID: u8 = 0x6;

/// Write `timestamp` (and, if `with_id`, the id 0x6) into the clock object at `p`.
fn write_clock(d: &TypeDesc, p: *mut u8, timestamp: u64, with_id: bool) {
    // SAFETY: `p` is the clock allocation of `d.size` bytes made in `move_rt_sui_clock_borrow`.
    let mut fields = unsafe { Slot::new(d, p) }.fields();
    let mut id = fields.next().expect("Clock.id");
    if with_id {
        // UID -> ID -> address
        while id.desc().kind() == Kind::Struct {
            id = id.fields().next().expect("single-field wrapper");
        }
        let mut address = [0u8; 32];
        address[31] = CLOCK_ID;
        id.write_prim(&address);
    }
    fields
        .next()
        .expect("Clock.timestamp_ms")
        .write_prim(&timestamp.to_le_bytes());
}

/// `clock::native_borrow()`: the clock object (`d` is `Clock`'s descriptor).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_clock_borrow(d: &TypeDesc) -> *const u8 {
    let (p, ts) = CLOCK.with(|c| *c);
    if !p.is_null() {
        return p;
    }
    let p = desc::alloc(d.size() as usize);
    write_clock(d, p, ts, true);
    CLOCK.with(|c| c.0 = p);
    p
}

/// `clock::native_set_timestamp_ms_for_testing(ms)`.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_clock_set(d: &TypeDesc, timestamp_ms: u64) {
    let p = CLOCK.with(|c| {
        c.1 = timestamp_ms;
        c.0
    });
    if !p.is_null() {
        write_clock(d, p, timestamp_ms, false);
    }
}
