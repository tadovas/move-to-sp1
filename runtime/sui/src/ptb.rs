//! The PTB interpreter: runs a transaction (a programmable transaction block) against a
//! program image, whose callable functions are listed in the compiler-emitted dispatch table
//! (`compiler/src/translate/image.rs`). The transaction is input data; the image is code.
//!
//! Encoding of the PTB (one SP1 stdin buffer, BCS conventions; produced by the runner):
//! ```text
//! Ptb     = vector<Input>, vector<Command>
//! Input   = 0 Object { id: address } | 1 Pure { bytes: vector<u8> }
//! Command = 0 MoveCall { function: String, args: vector<Arg> }
//! Arg     = 0 GasCoin | 1 Input(u16) | 2 Result(u16) | 3 NestedResult(u16, u16)
//! ```
//! `function` is a dispatch key (`<address>::<module>::<function><type args>`). A trailing
//! `&TxContext` / `&mut TxContext` parameter is supplied implicitly, as on Sui.

use alloc::{vec, vec::Vec};

use move_rt_core::{
    abort_in, bcs,
    desc::{self, Slot, TypeDesc},
};

use crate::{Address, store};

const PTB: &str = "move2sp1::ptb";
const E_BAD_PTB: u64 = 1;
const E_UNKNOWN_FUNCTION: u64 = 2;
const E_BAD_ARGUMENT: u64 = 3;

const BY_VALUE: u64 = 0;

#[repr(C)]
struct Param {
    kind: u64,
    desc: *const TypeDesc,
}

#[repr(C)]
struct Entry {
    key: *const u8,
    key_len: u64,
    wrapper: extern "C" fn(*const *mut u8, *const *mut u8),
    n_params: u64,
    params: *const Param,
    n_returns: u64,
    returns: *const *const TypeDesc,
}

unsafe extern "C" {
    /// Emitted by the compiler: `__move_dispatch_len` entries (empty unless an image).
    static __move_dispatch: [Entry; 0];
    static __move_dispatch_len: u64;
}

impl Entry {
    fn key(&self) -> &'static [u8] {
        // SAFETY: dispatch keys are constants of `key_len` bytes.
        unsafe { core::slice::from_raw_parts(self.key, self.key_len as usize) }
    }
    fn params(&self) -> &'static [Param] {
        // SAFETY: constant arrays of the stated length (possibly empty).
        if self.n_params == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.params, self.n_params as usize) }
        }
    }
    fn returns(&self) -> &'static [*const TypeDesc] {
        // SAFETY: as above.
        if self.n_returns == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.returns, self.n_returns as usize) }
        }
    }
}

fn dispatch(key: &[u8]) -> &'static Entry {
    // SAFETY: the compiler emits an array of `__move_dispatch_len` entries.
    let table = unsafe {
        core::slice::from_raw_parts(__move_dispatch.as_ptr(), __move_dispatch_len as usize)
    };
    table
        .iter()
        .find(|e| e.key() == key)
        .unwrap_or_else(|| abort_in(PTB, E_UNKNOWN_FUNCTION))
}

fn desc_of(p: *const TypeDesc) -> &'static TypeDesc {
    // SAFETY: dispatch descriptors are the program's constant descriptors.
    unsafe { &*p }
}

enum Input<'a> {
    Object(Address),
    Pure(&'a [u8]),
}

enum Arg {
    GasCoin,
    Input(u16),
    Result(u16, u16),
}

/// Run the PTB (next stdin buffer) with the transaction context at `ctx`.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_run_ptb(ctx: *mut u8) {
    let buf: Vec<u8> = sp1_zkvm::io::read_vec();
    let mut r = Reader(&buf);
    let inputs: Vec<Input> = (0..r.uleb())
        .map(|_| match r.byte() {
            0 => Input::Object(r.address()),
            1 => Input::Pure(r.bytes()),
            _ => abort_in(PTB, E_BAD_PTB),
        })
        .collect();
    // results[command][i]: values produced by earlier commands (None once moved)
    let mut results: Vec<Vec<Option<(*mut u8, &'static TypeDesc)>>> = vec![];
    for _ in 0..r.uleb() {
        match r.byte() {
            0 => {
                let key = r.bytes();
                let args: Vec<Arg> = (0..r.uleb()).map(|_| r.arg()).collect();
                let produced = move_call(dispatch(key), &args, &inputs, &mut results, ctx);
                results.push(produced);
            }
            _ => abort_in(PTB, E_BAD_PTB),
        }
    }
    crate::store::move_rt_sui_check_inputs();
}

fn move_call(
    entry: &'static Entry,
    args: &[Arg],
    inputs: &[Input],
    results: &mut [Vec<Option<(*mut u8, &'static TypeDesc)>>],
    ctx: *mut u8,
) -> Vec<Option<(*mut u8, &'static TypeDesc)>> {
    let params = entry.params();
    // a trailing TxContext parameter is implicit
    let implicit_ctx = params.len() == args.len() + 1
        && params
            .last()
            .is_some_and(|p| desc_of(p.desc).name() == "tx_context::TxContext");
    if args.len() + implicit_ctx as usize != params.len() {
        abort_in(PTB, E_BAD_ARGUMENT);
    }
    let mut ptrs: Vec<*mut u8> = vec![];
    for (arg, p) in args.iter().zip(params) {
        let d = desc_of(p.desc);
        let by_value = p.kind == BY_VALUE;
        ptrs.push(match arg {
            Arg::Input(i) => match inputs.get(*i as usize) {
                Some(Input::Object(id)) => object_arg(id, d, by_value),
                Some(Input::Pure(bytes)) => pure_arg(bytes, d),
                None => abort_in(PTB, E_BAD_ARGUMENT),
            },
            Arg::Result(c, i) => {
                let slot = results
                    .get_mut(*c as usize)
                    .and_then(|r| r.get_mut(*i as usize));
                let Some(slot) = slot else {
                    abort_in(PTB, E_BAD_ARGUMENT)
                };
                let value = if by_value { slot.take() } else { *slot };
                match value {
                    Some((data, _)) => data,
                    None => abort_in(PTB, E_BAD_ARGUMENT), // already moved
                }
            }
            Arg::GasCoin => abort_in(PTB, E_BAD_ARGUMENT), // not yet supported
        });
    }
    if implicit_ctx {
        ptrs.push(ctx);
    }
    crate::store::move_rt_sui_check_inputs();
    let rets: Vec<(*mut u8, &'static TypeDesc)> = entry
        .returns()
        .iter()
        .map(|&d| {
            let d = desc_of(d);
            (desc::alloc(d.size() as usize), d)
        })
        .collect();
    let ret_ptrs: Vec<*mut u8> = rets.iter().map(|(p, _)| *p).collect();
    (entry.wrapper)(ptrs.as_ptr(), ret_ptrs.as_ptr());
    rets.into_iter().map(Some).collect()
}

/// An object input: lent from the state (`&`/`&mut`), or moved out of it (by value). The
/// clock (0x6) falls back to the runtime's clock singleton when not in the state.
fn object_arg(id: &Address, d: &TypeDesc, by_value: bool) -> *mut u8 {
    const CLOCK: Address = {
        let mut a = [0u8; 32];
        a[31] = 6;
        a
    };
    if *id == CLOCK && !store::has_object(id) {
        return crate::clock::move_rt_sui_clock_borrow(d) as *mut u8;
    }
    if by_value {
        store::take_object(id, d)
    } else {
        store::move_rt_sui_borrow_object(id, d)
    }
}

/// A pure input, decoded with the parameter's type into a fresh slot.
fn pure_arg(bytes: &[u8], d: &TypeDesc) -> *mut u8 {
    let data = desc::alloc(d.size() as usize);
    let mut input = bytes;
    // SAFETY: `data` is a fresh allocation of `d.size` bytes.
    let slot = unsafe { Slot::new(d, data) };
    if bcs::decode(slot, &mut input).is_err() || !input.is_empty() {
        abort_in(PTB, E_BAD_ARGUMENT);
    }
    data
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> &'a [u8] {
        if self.0.len() < n {
            abort_in(PTB, E_BAD_PTB);
        }
        let (h, t) = self.0.split_at(n);
        self.0 = t;
        h
    }
    fn byte(&mut self) -> u8 {
        self.take(1)[0]
    }
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.take(2).try_into().unwrap())
    }
    fn address(&mut self) -> Address {
        self.take(32).try_into().unwrap()
    }
    fn uleb(&mut self) -> u64 {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte();
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return v;
            }
        }
        abort_in(PTB, E_BAD_PTB)
    }
    fn bytes(&mut self) -> &'a [u8] {
        let n = self.uleb() as usize;
        self.take(n)
    }
    fn arg(&mut self) -> Arg {
        match self.byte() {
            0 => Arg::GasCoin,
            1 => Arg::Input(self.u16()),
            2 => Arg::Result(self.u16(), 0),
            3 => {
                let c = self.u16();
                Arg::Result(c, self.u16())
            }
            _ => abort_in(PTB, E_BAD_PTB),
        }
    }
}
