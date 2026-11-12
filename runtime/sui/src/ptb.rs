//! The PTB interpreter: runs a transaction (a programmable transaction block) against a
//! program image, whose callable functions are listed in the compiler-emitted dispatch table
//! (`compiler/src/translate/image.rs`). The transaction is input data; the image is code.
//!
//! Encoding of the PTB (one SP1 stdin buffer, BCS conventions; produced by the runner):
//! ```text
//! Ptb     = Gas, vector<Input>, vector<Command>
//! Gas     = price: u64, rgp: u64, budget: u64, coins: vector<address>, charge: u64, refund: u64,
//!           owner: address
//! Input   = 0 Object { id: address } | 1 Pure { bytes: vector<u8> }
//!         | 2 Reservation { coin_type: String, amount: u64 }
//! Command = 0 MoveCall { function: String, args: vector<Arg> }
//!         | 1 TransferObjects { objects: vector<Arg>, address: Arg }
//!         | 2 SplitCoins { coin: Arg, amounts: vector<Arg> }
//!         | 3 MergeCoins { coin: Arg, sources: vector<Arg> }
//!         | 4 MakeMoveVec { type: Option<String>, elements: vector<Arg> }
//! Arg     = 0 GasCoin | 1 Input(u16) | 2 Result(u16) | 3 NestedResult(u16, u16)
//! ```
//! `function` is a dispatch key (`<address>::<module>::<function><type args>`). A trailing
//! `&TxContext` / `&mut TxContext` parameter is supplied implicitly, as on Sui.
//!
//! Gas: `price` / `rgp` / `budget` are what `tx_context` reports. Gas is not metered: the gas
//! coins are smashed into the first (as Sui does) and, after the commands, `charge` is debited
//! from it and `refund` credited — amounts the caller supplies (e.g. from chain effects).
//! A zero first coin stands for the sender's address balance (Sui's gas reservation): the
//! other coins are absorbed into it (deleted), and `GasCoin` is a virtual `Coin<SUI>` holding
//! them plus the balance. Address balances are not modelled: the virtual coin's balance part
//! is an unchecked allowance, and what is left of it is discarded. The gas coin may belong to a sponsor: using it is not subject to
//! the sender-ownership check.
//!
//! A reservation (Sui's coin reservation: an object reference standing for `amount` withdrawn
//! from the sender's address balance) becomes a fresh `Coin<T>` of that value, owned by the
//! sender; the withdrawal is recorded, and what is left of the coin if the transaction does not
//! move it goes back to the balance at the end.
//!
//! Values are typed when they are used: pure bytes are decoded, and vectors built by
//! `MakeMoveVec` are laid out, with the type of the parameter that consumes them.

use alloc::{vec, vec::Vec};

use move_rt_core::{
    abort_in, bcs,
    desc::{self, Kind, Slot, TypeDesc},
};

use crate::{Address, state, store};

const PTB: &str = "move2sp1::ptb";
const E_BAD_PTB: u64 = 1;
const E_UNKNOWN_FUNCTION: u64 = 2;
const E_BAD_ARGUMENT: u64 = 3;
const E_TYPE_MISMATCH: u64 = 4;
const E_NOT_A_COIN: u64 = 5;
const E_INSUFFICIENT_COIN_BALANCE: u64 = 6;
const E_COIN_BALANCE_OVERFLOW: u64 = 7;
const E_NO_GAS_COIN: u64 = 8;
const E_UNREGISTERED_TYPE: u64 = 9;
const E_INSUFFICIENT_GAS: u64 = 10;

const BY_VALUE: u64 = 0;

/// `0x2::coin::Coin<`, canonically.
const COIN_PREFIX: &[u8] =
    b"0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<";

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
    /// Coin type (canonical `Coin<T>`), amount: materialized by `reserve_coins`.
    Reservation(&'a [u8], u64),
}

#[derive(Clone, Copy)]
enum Arg {
    GasCoin,
    Input(u16),
    Result(u16, u16),
}

/// A value produced or referenced by the transaction.
#[derive(Clone)]
enum Val<'a> {
    /// In memory, of this type.
    Data(*mut u8, &'static TypeDesc),
    /// BCS bytes, typed when used.
    Pure(&'a [u8]),
    /// A vector built by `MakeMoveVec`, laid out when used.
    Vec(Vec<Val<'a>>),
}

struct Interp<'a> {
    inputs: Vec<Input<'a>>,
    /// results[command][i]: values produced by earlier commands (None once moved)
    results: Vec<Vec<Option<Val<'a>>>>,
    gas_coin: Option<Address>,
    /// The virtual gas coin of a gas reservation.
    virtual_gas: Option<(*mut u8, &'static TypeDesc)>,
    /// Who pays for gas (the sender, or a sponsor).
    gas_owner: Address,
    ctx: *mut u8,
}

/// Balance of the virtual gas coin beyond the absorbed coins (address balances are not
/// modelled, so withdrawals from them are not checked).
const ADDRESS_BALANCE_ALLOWANCE: u64 = 1 << 62;

/// `0x2::balance::Balance<0x2::sui::SUI>`, canonically.
const SUI_BALANCE: &[u8] = b"0000000000000000000000000000000000000000000000000000000000000002::balance::Balance<0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

/// `0x2::coin::Coin<0x2::sui::SUI>`, canonically.
const SUI_COIN: &[u8] = b"0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

/// Run the PTB (next stdin buffer) with the transaction context at `ctx`.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_run_ptb(ctx: *mut u8) {
    let buf: Vec<u8> = sp1_zkvm::io::read_vec();
    let mut r = Reader(&buf);
    let (price, rgp, budget) = (r.u64(), r.u64(), r.u64());
    crate::tx::set_gas(price, rgp, budget);
    let gas_coins: Vec<Address> = (0..r.uleb()).map(|_| r.address()).collect();
    let (charge, refund) = (r.u64(), r.u64());
    let gas_owner = r.address();
    crate::tx::set_gas_owner(&gas_owner);
    let inputs: Vec<Input> = (0..r.uleb())
        .map(|_| match r.byte() {
            0 => Input::Object(r.address()),
            1 => Input::Pure(r.bytes()),
            2 => Input::Reservation(r.bytes(), r.u64()),
            _ => abort_in(PTB, E_BAD_PTB),
        })
        .collect();
    let mut t = Interp {
        inputs,
        results: vec![],
        gas_coin: gas_coins.first().copied(),
        virtual_gas: None,
        gas_owner,
        ctx,
    };
    t.smash_gas(&gas_coins);
    let reserved = t.reserve_coins();
    for _ in 0..r.uleb() {
        let produced = match r.byte() {
            0 => {
                let key = r.bytes();
                let args = r.args();
                t.move_call(dispatch(key), &args)
            }
            1 => {
                let objects = r.args();
                let address = r.arg();
                t.transfer_objects(&objects, address)
            }
            2 => {
                let coin = r.arg();
                let amounts = r.args();
                t.split_coins(coin, &amounts)
            }
            3 => {
                let coin = r.arg();
                let sources = r.args();
                t.merge_coins(coin, &sources)
            }
            4 => {
                // the element type only matters once the vector is used
                if r.byte() == 1 {
                    r.bytes();
                }
                let elements = r.args();
                let elems = elements.iter().map(|a| t.take(*a)).collect();
                vec![Val::Vec(elems)]
            }
            _ => abort_in(PTB, E_BAD_PTB),
        };
        t.results.push(produced.into_iter().map(Some).collect());
    }
    t.return_reservations(&reserved);
    t.charge_gas(charge, refund);
    store::move_rt_sui_check_inputs();
}

impl<'a> Interp<'a> {
    /// A value moved out of an argument (objects leave the state).
    fn take(&mut self, arg: Arg) -> Val<'a> {
        self.get(arg, true)
    }

    /// A value borrowed from an argument (objects stay in the state; mutations persist).
    fn borrow(&mut self, arg: Arg) -> Val<'a> {
        self.get(arg, false)
    }

    fn get(&mut self, arg: Arg, by_value: bool) -> Val<'a> {
        let object = |id: &Address| {
            let Some(d) = store::object_desc(id) else {
                abort_in(crate::location::STATE, state::E_OBJECT_NOT_IN_STATE)
            };
            let p = if by_value {
                store::take_object(id, d)
            } else {
                store::move_rt_sui_borrow_object(id, d)
            };
            Val::Data(p, d)
        };
        match arg {
            Arg::GasCoin => {
                if let Some((p, d)) = self.virtual_gas {
                    if by_value {
                        abort_in(PTB, E_BAD_ARGUMENT); // the address balance cannot be moved
                    }
                    return Val::Data(p, d);
                }
                let id = self
                    .gas_coin
                    .unwrap_or_else(|| abort_in(PTB, E_NO_GAS_COIN));
                let (p, d) = gas_coin(&id);
                if by_value {
                    // moved out, e.g. transferred whole: gas is still charged to it after
                    store::delete_object(&id);
                }
                Val::Data(p, d)
            }
            Arg::Input(i) => match self.inputs.get(i as usize) {
                Some(Input::Object(id)) => object(id),
                Some(Input::Pure(bytes)) => Val::Pure(bytes),
                // materialized into objects before the commands run
                Some(Input::Reservation(..)) | None => abort_in(PTB, E_BAD_ARGUMENT),
            },
            Arg::Result(c, i) => {
                let slot = self.result_slot(c, i);
                let v = if by_value { slot.take() } else { slot.clone() };
                v.unwrap_or_else(|| abort_in(PTB, E_BAD_ARGUMENT)) // already moved
            }
        }
    }

    fn result_slot(&mut self, c: u16, i: u16) -> &mut Option<Val<'a>> {
        self.results
            .get_mut(c as usize)
            .and_then(|r| r.get_mut(i as usize))
            .unwrap_or_else(|| abort_in(PTB, E_BAD_ARGUMENT))
    }

    fn move_call(&mut self, entry: &'static Entry, args: &[Arg]) -> Vec<Val<'a>> {
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
            ptrs.push(self.typed_arg(*arg, desc_of(p.desc), p.kind == BY_VALUE));
        }
        if implicit_ctx {
            ptrs.push(self.ctx);
        }
        store::move_rt_sui_check_inputs();
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
        rets.into_iter().map(|(p, d)| Val::Data(p, d)).collect()
    }

    /// An argument for a parameter of type `d`.
    fn typed_arg(&mut self, arg: Arg, d: &'static TypeDesc, by_value: bool) -> *mut u8 {
        match arg {
            // objects are looked up with the parameter's type (and the clock may be implicit)
            Arg::Input(_) if !matches!(self.input(arg), Some(Input::Pure(_))) => {
                let Some(Input::Object(id)) = self.input(arg) else {
                    abort_in(PTB, E_BAD_ARGUMENT)
                };
                object_arg(&{ *id }, d, by_value)
            }
            Arg::Result(c, i) if !by_value => {
                // a borrowed result is laid out once, in place, so mutations persist
                let slot = self.result_slot(c, i);
                let v = slot.take().unwrap_or_else(|| abort_in(PTB, E_BAD_ARGUMENT));
                let p = materialize(v, d);
                *slot = Some(Val::Data(p, d));
                p
            }
            _ => {
                let v = self.get(arg, by_value);
                materialize(v, d)
            }
        }
    }

    fn input(&self, arg: Arg) -> Option<&Input<'a>> {
        match arg {
            Arg::Input(i) => self.inputs.get(i as usize),
            _ => None,
        }
    }

    fn transfer_objects(&mut self, objects: &[Arg], address: Arg) -> Vec<Val<'a>> {
        let recipient: Address = match self.take(address) {
            Val::Pure(b) => b
                .try_into()
                .unwrap_or_else(|_| abort_in(PTB, E_BAD_ARGUMENT)),
            Val::Data(p, d) if d.kind() == Kind::Address => {
                // SAFETY: an address value is 32 bytes.
                unsafe { *(p as *const Address) }
            }
            _ => abort_in(PTB, E_BAD_ARGUMENT),
        };
        for o in objects {
            let Val::Data(p, d) = self.take(*o) else {
                abort_in(PTB, E_BAD_ARGUMENT)
            };
            let name =
                state::canonical_name(d).unwrap_or_else(|| abort_in(PTB, E_UNREGISTERED_TYPE));
            // SAFETY: `p` holds an object of type `d`; `name` is a constant.
            unsafe {
                crate::effects::move_rt_sui_transfer(
                    d,
                    name.as_ptr(),
                    name.len() as u64,
                    p,
                    &recipient,
                )
            };
        }
        vec![]
    }

    fn split_coins(&mut self, coin: Arg, amounts: &[Arg]) -> Vec<Val<'a>> {
        let Val::Data(p, d) = self.borrow(coin) else {
            abort_in(PTB, E_BAD_ARGUMENT)
        };
        let mut out = vec![];
        for a in amounts {
            let amount = match self.take(*a) {
                Val::Pure(b) => u64::from_le_bytes(
                    b.try_into()
                        .unwrap_or_else(|_| abort_in(PTB, E_BAD_ARGUMENT)),
                ),
                Val::Data(q, qd) if qd.kind() == Kind::U64 => {
                    // SAFETY: a u64 value.
                    unsafe { *(q as *const u64) }
                }
                _ => abort_in(PTB, E_BAD_ARGUMENT),
            };
            let value = coin_value(p, d);
            if *value < amount {
                abort_in(PTB, E_INSUFFICIENT_COIN_BALANCE);
            }
            *value -= amount;
            // a new coin (`coin::split`): fresh UID, the split balance
            let new = desc::alloc(d.size() as usize);
            let id_offset = d.fields()[0].offset();
            // SAFETY: `new` has room for a `Coin`; its UID is a 32-byte address at `id_offset`.
            unsafe { *(new.add(id_offset) as *mut Address) = crate::tx::new_id() };
            *coin_value(new, d) = amount;
            out.push(Val::Data(new, d));
        }
        out
    }

    fn merge_coins(&mut self, coin: Arg, sources: &[Arg]) -> Vec<Val<'a>> {
        let Val::Data(p, d) = self.borrow(coin) else {
            abort_in(PTB, E_BAD_ARGUMENT)
        };
        coin_value(p, d);
        for s in sources {
            let Val::Data(q, qd) = self.take(*s) else {
                abort_in(PTB, E_BAD_ARGUMENT)
            };
            if !core::ptr::eq(d, qd) {
                abort_in(PTB, E_TYPE_MISMATCH);
            }
            let add = *coin_value(q, qd);
            let value = coin_value(p, d);
            *value = value
                .checked_add(add)
                .unwrap_or_else(|| abort_in(PTB, E_COIN_BALANCE_OVERFLOW));
            // the source coin's UID is deleted with it: it is no longer in the state
        }
        vec![]
    }

    /// Coin reservations become coins owned by the sender (fresh IDs, in input order).
    fn reserve_coins(&mut self) -> Vec<(Address, &'static TypeDesc)> {
        let sender = crate::tx::sender();
        let mut reserved = vec![];
        for input in self.inputs.iter_mut() {
            let Input::Reservation(name, amount) = *input else {
                continue;
            };
            let d = state::desc_by_name(name).unwrap_or_else(|| abort_in(PTB, E_UNREGISTERED_TYPE));
            let p = desc::alloc(d.size() as usize);
            let id = crate::tx::new_id();
            // SAFETY: `p` has room for a `Coin`; its UID is a 32-byte address at offset 0.
            unsafe { *(p.add(d.fields()[0].offset()) as *mut Address) = id };
            *coin_value(p, d) = amount;
            record_balance(&sender, d, crate::accumulator::SPLIT, amount);
            // SAFETY: `p` holds a `Coin` of type `d`.
            unsafe { store::put(Some(sender), d, p) };
            *input = Input::Object(id);
            reserved.push((id, d));
        }
        reserved
    }

    /// What is left of reserved coins the transaction did not move returns to the balance.
    fn return_reservations(&mut self, reserved: &[(Address, &'static TypeDesc)]) {
        let sender = crate::tx::sender();
        for (id, d) in reserved {
            let Some((p, pd)) = store::object_in_place(id) else {
                continue; // moved: a coin from now on
            };
            if !core::ptr::eq(pd, *d) || store::owner_of_object(id) != Some(Some(sender)) {
                continue;
            }
            record_balance(&sender, d, crate::accumulator::MERGE, *coin_value(p, d));
            store::delete_object(id);
        }
    }

    /// Gas smashing: the other gas coins are merged into the first and deleted.
    fn smash_gas(&mut self, coins: &[Address]) {
        let Some((first, rest)) = coins.split_first() else {
            return;
        };
        if *first == [0; 32] {
            // gas from the address balance: the coins go into it
            let d = state::desc_by_name(SUI_COIN).unwrap_or_else(|| abort_in(PTB, E_NO_GAS_COIN));
            let p = desc::alloc(d.size() as usize);
            // Sui materializes it as a coin object: its UID is the transaction's first new ID
            let id_offset = d.fields()[0].offset();
            // SAFETY: `p` has room for a `Coin`; its UID is a 32-byte address at `id_offset`.
            unsafe { *(p.add(id_offset) as *mut Address) = crate::tx::new_id() };
            let mut total = ADDRESS_BALANCE_ALLOWANCE;
            for c in rest {
                let (q, qd) = gas_coin(c);
                total += *coin_value(q, qd);
                store::delete_object(c);
            }
            *coin_value(p, d) = total;
            self.gas_coin = None;
            self.virtual_gas = Some((p, d));
            return;
        }
        let (p, d) = gas_coin(first);
        for c in rest {
            let (q, qd) = gas_coin(c);
            if !core::ptr::eq(d, qd) {
                abort_in(PTB, E_TYPE_MISMATCH);
            }
            let add = *coin_value(q, qd);
            let value = coin_value(p, d);
            *value = value
                .checked_add(add)
                .unwrap_or_else(|| abort_in(PTB, E_COIN_BALANCE_OVERFLOW));
            store::delete_object(c);
        }
    }

    /// Debit the (caller-supplied) gas charge from the gas coin, credit the rebate.
    fn charge_gas(&mut self, charge: u64, refund: u64) {
        if let Some((p, d)) = self.virtual_gas {
            // what is left of the virtual gas coin returns to the address balance: the net
            // change of the balance, beyond gas (charged by the caller), is recorded
            let left = *coin_value(p, d) as i128 - ADDRESS_BALANCE_ALLOWANCE as i128;
            let (op, amount) = if left >= 0 {
                (crate::accumulator::MERGE, left as u64)
            } else {
                (crate::accumulator::SPLIT, (-left) as u64)
            };
            if amount != 0 {
                // SAFETY: a constant name.
                unsafe {
                    crate::effects::record_accumulator(
                        SUI_BALANCE.as_ptr(),
                        SUI_BALANCE.len() as u64,
                        &self.gas_owner,
                        op,
                        amount,
                    )
                };
            }
            return;
        }
        let Some(id) = self.gas_coin else {
            return;
        };
        let (p, d) = gas_coin(&id);
        let value = coin_value(p, d);
        *value = (*value + refund)
            .checked_sub(charge)
            .unwrap_or_else(|| abort_in(PTB, E_INSUFFICIENT_GAS));
    }
}

/// Record an address-balance change for the balance type of coin type `d`.
fn record_balance(owner: &Address, d: &TypeDesc, op: u8, amount: u64) {
    let coin = state::canonical_name(d).unwrap_or_else(|| abort_in(PTB, E_UNREGISTERED_TYPE));
    // `<0x2>::coin::Coin<T>` -> `<0x2>::balance::Balance<T>`
    let mut name: Vec<u8> = COIN_PREFIX[..66].to_vec();
    name.extend_from_slice(b"balance::Balance<");
    name.extend_from_slice(&coin[COIN_PREFIX.len()..]);
    // SAFETY: `name` is `name.len()` bytes.
    unsafe {
        crate::effects::record_accumulator(name.as_ptr(), name.len() as u64, owner, op, amount)
    };
}

fn gas_coin(id: &Address) -> (*mut u8, &'static TypeDesc) {
    store::object_in_place(id).unwrap_or_else(|| abort_in(PTB, E_NO_GAS_COIN))
}

/// The balance of a `Coin<T>` at `p` (`coin.balance.value`).
fn coin_value<'b>(p: *mut u8, d: &TypeDesc) -> &'b mut u64 {
    if !state::canonical_name(d).is_some_and(|n| n.starts_with(COIN_PREFIX)) {
        abort_in(PTB, E_NOT_A_COIN);
    }
    // Coin { id: UID, balance: Balance<T> { value: u64 } }
    let balance = &d.fields()[1];
    let value = &balance.ty().fields()[0];
    // SAFETY: `p` holds a `Coin` described by `d`; the offsets come from its descriptor.
    unsafe { &mut *(p.add(balance.offset() + value.offset()) as *mut u64) }
}

/// Lay out `v` as a value of type `d`.
fn materialize(v: Val, d: &'static TypeDesc) -> *mut u8 {
    match v {
        Val::Data(p, vd) => {
            if !core::ptr::eq(vd, d) {
                abort_in(PTB, E_TYPE_MISMATCH);
            }
            p
        }
        Val::Pure(bytes) => pure_arg(bytes, d),
        Val::Vec(elems) => {
            if d.kind() != Kind::Vector {
                abort_in(PTB, E_TYPE_MISMATCH);
            }
            let data = desc::alloc(d.size() as usize);
            let e = d.elem();
            // SAFETY: `data` is a fresh allocation for a value of type `d`.
            let slot = unsafe { Slot::new(d, data) };
            for (s, v) in slot.init_vector(elems.len()).zip(elems) {
                let p = materialize(v, e);
                // SAFETY: `p` holds a value of the element type, moved into the vector.
                unsafe { s.move_from(p) };
            }
            data
        }
    }
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
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take(8).try_into().unwrap())
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
    fn args(&mut self) -> Vec<Arg> {
        (0..self.uleb()).map(|_| self.arg()).collect()
    }
}
