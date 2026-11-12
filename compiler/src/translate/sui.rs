//! Sui framework (0x2) natives. The runtime side is `runtime/sui` (`move-rt-sui`):
//! transaction context, the dynamic-field object store, and the effects log.
//!
//! Addresses cross into the runtime by pointer to their 32 bytes (they are `i256` values
//! here, which have no portable C ABI); generic objects by pointer plus type descriptor.

use anyhow::Result;
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, IntValue};

use super::{FnInst, Translator, function::Frame, types::Type};

/// Canonical name of `sui::tx_context::TxContext`.
pub(super) const TX_CONTEXT: &str =
    "0000000000000000000000000000000000000000000000000000000000000002::tx_context::TxContext";

impl<'ctx> Translator<'ctx, '_> {
    /// Returns false if `name` is not a supported Sui native.
    pub(super) fn sui_native(
        &mut self,
        fr: &mut Frame<'ctx>,
        f: &FnInst,
        name: &str,
    ) -> Result<bool> {
        let t = |i: usize| f.type_args[i].clone();
        let i64t = self.ctx.i64_type();
        match name {
            // ---- tx_context: read from the transaction context the program received
            "tx_context::native_sender" => {
                let a = self.address_from_runtime("move_rt_sui_sender", vec![])?;
                fr.push(a, Type::Address);
            }
            "tx_context::fresh_id" => {
                let a = self.address_from_runtime(self.id_fn("move_rt_sui_fresh_id"), vec![])?;
                fr.push(a, Type::Address);
            }
            "tx_context::native_epoch"
            | "tx_context::native_epoch_timestamp_ms"
            | "tx_context::native_ids_created" => {
                let rt = match name {
                    "tx_context::native_epoch" => "move_rt_sui_epoch",
                    "tx_context::native_epoch_timestamp_ms" => "move_rt_sui_epoch_timestamp_ms",
                    _ => "move_rt_sui_ids_created",
                };
                let v = self.call_runtime(rt, Some(i64t.into()), &[])?.unwrap();
                fr.push(v, Type::U64);
            }

            // ---- object
            "object::borrow_uid" => {
                // `UID` is an object's first field: the object's address is the UID's.
                let (obj, _) = fr.pop()?;
                let (_, returns) = self.signature(f)?;
                fr.push(obj, returns[0].clone());
            }
            // object lifetimes are not tracked beyond the run
            "object::delete_impl"
            | "object::record_new_uid"
            | "object::record_new_uid_from_hash" => {
                let (params, _) = self.signature(f)?;
                fr.pop_n(params.len())?;
            }

            // ---- effects
            "event::emit" => {
                let (v, _) = fr.pop()?;
                self.effect("move_rt_sui_emit", &t(0), v, None)?;
            }
            "transfer::transfer_impl" => {
                let (recipient, _) = fr.pop()?;
                let (obj, _) = fr.pop()?;
                self.effect("move_rt_sui_transfer", &t(0), obj, Some(recipient))?;
            }
            "transfer::share_object_impl" | "transfer::freeze_object_impl" => {
                let (obj, _) = fr.pop()?;
                let rt = if name.contains("share") {
                    "move_rt_sui_share"
                } else {
                    "move_rt_sui_freeze"
                };
                self.effect(rt, &t(0), obj, None)?;
            }

            // ---- dynamic fields (child objects in the runtime's store)
            "dynamic_field::hash_type_and_key" => {
                let (key, _) = fr.pop()?;
                let (parent, _) = fr.pop()?;
                // Sui hashes the key's BCS type tag; the key type is known here, so its
                // encoding is a constant.
                let tag = bcs::to_bytes(&t(0).type_tag(self.prog)?)?;
                let (tag_ptr, tag_len) = self.const_bytes(&tag);
                let args = vec![
                    self.spill(parent)?.into(),
                    self.type_desc(&t(0))?.into(),
                    self.spill(key)?.into(),
                    tag_ptr.into(),
                    i64t.const_int(tag_len, false).into(),
                ];
                let id = self.address_from_runtime(self.id_fn("move_rt_sui_df_hash"), args)?;
                fr.push(id, Type::Address);
            }
            "dynamic_field::add_child_object" => {
                self.note_state_type(&t(0));
                let (child, _) = fr.pop()?;
                let (parent, _) = fr.pop()?;
                let args = [
                    self.spill(parent)?.into(),
                    self.type_desc(&t(0))?.into(),
                    self.spill(child)?.into(),
                ];
                self.call_runtime("move_rt_sui_df_add", None, &args)?;
            }
            "dynamic_field::borrow_child_object" | "dynamic_field::borrow_child_object_mut" => {
                self.note_state_type(&t(0));
                let (id, _) = fr.pop()?;
                let (parent_uid, _) = fr.pop()?; // &UID: points at the parent's address
                let args = [
                    parent_uid.into(),
                    self.spill(id)?.into(),
                    self.type_desc(&t(0))?.into(),
                ];
                let child = self.call_runtime(
                    "move_rt_sui_df_borrow",
                    Some(self.ptr_type().into()),
                    &args,
                )?;
                fr.push(child.unwrap(), Type::Ref(Box::new(t(0))));
            }
            "dynamic_field::remove_child_object" => {
                self.note_state_type(&t(0));
                let (id, _) = fr.pop()?;
                let (parent, _) = fr.pop()?;
                let lty = self.llvm_type(&t(0))?;
                let out = self.builder.build_alloca(lty, "child")?;
                let args = [
                    self.spill(parent)?.into(),
                    self.spill(id)?.into(),
                    self.type_desc(&t(0))?.into(),
                    out.into(),
                ];
                self.call_runtime("move_rt_sui_df_remove", None, &args)?;
                fr.push(self.builder.build_load(lty, out, "")?, t(0));
            }
            "dynamic_field::has_child_object" | "dynamic_field::has_child_object_with_ty" => {
                let (id, _) = fr.pop()?;
                let (parent, _) = fr.pop()?;
                let desc = match f.type_args.first() {
                    Some(ty) => self.type_desc(ty)?,
                    None => self.ptr_type().const_null(),
                };
                let args = [
                    self.spill(parent)?.into(),
                    self.spill(id)?.into(),
                    desc.into(),
                ];
                let has = self.runtime_bool("move_rt_sui_df_has", &args)?;
                fr.push(has, Type::Bool);
            }

            // ---- clock: a singleton object in the runtime
            "clock::native_borrow" => {
                let clock = self.clock_type()?;
                let args = [self.type_desc(&clock)?.into()];
                let p = self.call_runtime(
                    "move_rt_sui_clock_borrow",
                    Some(self.ptr_type().into()),
                    &args,
                )?;
                fr.push(p.unwrap(), Type::Ref(Box::new(clock)));
            }
            "clock::native_set_timestamp_ms_for_testing" => {
                let (ts, _) = fr.pop()?;
                let args = [self.type_desc(&self.clock_type()?)?.into(), ts.into()];
                self.call_runtime("move_rt_sui_clock_set", None, &args)?;
            }

            // ---- tx_context: gas parameters and the test-only `replace`
            "tx_context::native_rgp"
            | "tx_context::native_gas_price"
            | "tx_context::native_gas_budget" => {
                let rt = match name {
                    "tx_context::native_rgp" => "move_rt_sui_rgp",
                    "tx_context::native_gas_price" => "move_rt_sui_gas_price",
                    _ => "move_rt_sui_gas_budget",
                };
                let v = self.call_runtime(rt, Some(i64t.into()), &[])?.unwrap();
                fr.push(v, Type::U64);
            }
            "tx_context::derive_id" => {
                let (ids_created, _) = fr.pop()?;
                let (tx_hash, _) = fr.pop()?; // vector<u8>
                let args = vec![self.spill(tx_hash)?.into(), ids_created.into()];
                let a = self.address_from_runtime("move_rt_sui_derive_id", args)?;
                fr.push(a, Type::Address);
            }
            "tx_context::native_sponsor" => {
                // the gas owner of a PTB when not the sender (none outside PTBs)
                let v = self.runtime_bytes("move_rt_sui_sponsor", vec![])?;
                fr.push(v, Type::Vector(Box::new(Type::Address)));
            }
            "tx_context::replace" => {
                // (sender, tx_hash, epoch, epoch_timestamp_ms, ids_created, rgp, gas_price,
                //  gas_budget, sponsor); the sponsor is dropped
                let mut a = fr.pop_n(9)?.into_iter().map(|(v, _)| v);
                let (sender, tx_hash) = (a.next().unwrap(), a.next().unwrap());
                let mut args: Vec<BasicMetadataValueEnum> =
                    vec![self.spill(sender)?.into(), self.spill(tx_hash)?.into()];
                args.extend(a.take(6).map(BasicMetadataValueEnum::from));
                self.call_runtime("move_rt_sui_tx_replace", None, &args)?;
            }

            // ---- test_scenario: shared objects across transactions
            // `owner` below: null for shared objects, else a pointer to the account address
            "test_scenario::take_shared_by_id" | "test_scenario::take_from_address_by_id" => {
                self.note_state_type(&t(0));
                let (id, _) = fr.pop()?;
                let owner = if name.ends_with("shared_by_id") {
                    self.ptr_type().const_null()
                } else {
                    self.spill(fr.pop()?.0)?
                };
                fr.pop()?; // &Scenario
                let lty = self.llvm_type(&t(0))?;
                let out = self.builder.build_alloca(lty, "taken")?;
                let args = [
                    owner.into(),
                    self.spill(id)?.into(),
                    self.type_desc(&t(0))?.into(),
                    out.into(),
                ];
                self.call_runtime("move_rt_sui_take", None, &args)?;
                fr.push(self.builder.build_load(lty, out, "")?, t(0));
            }
            "test_scenario::was_taken_shared" | "test_scenario::was_taken_from_address" => {
                let (id, _) = fr.pop()?;
                let owner = if name.ends_with("shared") {
                    self.ptr_type().const_null()
                } else {
                    self.spill(fr.pop()?.0)?
                };
                let taken = self.runtime_bool(
                    "move_rt_sui_was_taken",
                    &[owner.into(), self.spill(id)?.into()],
                )?;
                fr.push(taken, Type::Bool);
            }
            "test_scenario::most_recent_id_shared"
            | "test_scenario::most_recent_id_for_address" => {
                let owner = if name.ends_with("shared") {
                    self.ptr_type().const_null()
                } else {
                    self.spill(fr.pop()?.0)?
                };
                let (_, returns) = self.signature(f)?; // Option<ID>
                let lty = self.llvm_type(&returns[0])?;
                let out = self.builder.build_alloca(lty, "most_recent")?;
                let args = [
                    owner.into(),
                    self.type_desc(&t(0))?.into(),
                    self.type_desc(&returns[0])?.into(),
                    out.into(),
                ];
                self.call_runtime("move_rt_sui_most_recent", None, &args)?;
                fr.push(self.builder.build_load(lty, out, "")?, returns[0].clone());
            }
            "test_scenario::end_transaction" => {
                // effects summaries are not tracked: an empty `TransactionEffects` (all-zero
                // memory is a valid value: empty vectors and maps, zero counts)
                self.call_runtime("move_rt_sui_end_transaction", None, &[])?;
                let (_, returns) = self.signature(f)?;
                let empty = self.llvm_type(&returns[0])?.const_zero();
                fr.push(empty, returns[0].clone());
            }

            // ---- hashing
            "hash::blake2b256" | "hash::keccak256" => {
                let (data, _) = fr.pop()?; // &vector<u8>
                let rt = if name.ends_with("blake2b256") {
                    "move_rt_sui_blake2b256"
                } else {
                    "move_rt_sui_keccak256"
                };
                let digest = self.runtime_bytes(rt, vec![data.into()])?;
                fr.push(digest, Type::Vector(Box::new(Type::U8)));
            }

            // ---- address balances (funds accumulators; runtime/sui/src/accumulator.rs)
            "funds_accumulator::add_to_accumulator_address" => {
                let (value, _) = fr.pop()?; // T
                let (recipient, _) = fr.pop()?;
                fr.pop()?; // the accumulator's address: derived from recipient and T
                let (name, len) = self.const_bytes(t(0).canonical(self.prog).as_bytes());
                let args = [
                    self.type_desc(&t(0))?.into(),
                    name.into(),
                    i64t.const_int(len, false).into(),
                    self.spill(value)?.into(),
                    self.spill(recipient)?.into(),
                ];
                self.call_runtime("move_rt_sui_accumulator_add", None, &args)?;
            }
            "accumulator::emit_deposit_event" | "accumulator::emit_withdraw_event" => {
                let (amount, _) = fr.pop()?; // u64
                let (recipient, _) = fr.pop()?;
                fr.pop()?; // accumulator address
                let op = if name.ends_with("deposit_event") {
                    0
                } else {
                    1
                }; // merge / split
                let (ty_name, len) = self.const_bytes(t(0).canonical(self.prog).as_bytes());
                let args = [
                    ty_name.into(),
                    i64t.const_int(len, false).into(),
                    self.spill(recipient)?.into(),
                    self.ctx.i8_type().const_int(op, false).into(),
                    amount.into(),
                ];
                self.call_runtime("move_rt_sui_accumulator_event", None, &args)?;
            }
            "config::read_setting_impl" => {
                // <Field<Name, Setting<V>>, Setting<V>, SettingData<V>, V>(config, name, epoch)
                self.note_state_type(&t(0));
                let (epoch, _) = fr.pop()?;
                let (name_id, _) = fr.pop()?;
                let (config, _) = fr.pop()?;
                let opt = Type::parse(
                    self.prog,
                    &format!("0x1::option::Option<{}>", t(3).canonical(self.prog)),
                )?;
                let lty = self.llvm_type(&opt)?;
                let out = self.builder.build_alloca(lty, "setting")?;
                let args = [
                    self.type_desc(&t(0))?.into(),
                    self.spill(config)?.into(),
                    self.spill(name_id)?.into(),
                    epoch.into(),
                    self.type_desc(&opt)?.into(),
                    out.into(),
                ];
                self.call_runtime("move_rt_sui_read_setting", None, &args)?;
                fr.push(self.builder.build_load(lty, out, "")?, opt);
            }
            "funds_accumulator::reserve_object_funds_for_withdrawal" => {
                // object balances are not modelled: the reservation is not checked
                fr.pop()?; // limit
                fr.pop()?; // owner
            }
            "funds_accumulator::withdraw_from_accumulator_address" => {
                let (value, _) = fr.pop()?; // u256
                let (owner, _) = fr.pop()?;
                fr.pop()?; // accumulator address
                let (name, len) = self.const_bytes(t(0).canonical(self.prog).as_bytes());
                let lty = self.llvm_type(&t(0))?;
                let out = self.builder.build_alloca(lty, "funds")?;
                let args = [
                    self.type_desc(&t(0))?.into(),
                    name.into(),
                    i64t.const_int(len, false).into(),
                    self.spill(owner)?.into(),
                    self.spill(value)?.into(),
                    out.into(),
                ];
                self.call_runtime("move_rt_sui_accumulator_withdraw", None, &args)?;
                fr.push(self.builder.build_load(lty, out, "")?, t(0));
            }

            // ---- signatures (runtime/sui/src/crypto.rs)
            "ecdsa_k1::secp256k1_ecrecover" | "ecdsa_r1::secp256r1_ecrecover" => {
                let (hash, _) = fr.pop()?; // u8
                let (msg, _) = fr.pop()?; // &vector<u8>
                let (sig, _) = fr.pop()?; // &vector<u8>
                let rt = if name.starts_with("ecdsa_k1") {
                    "move_rt_sui_secp256k1_ecrecover"
                } else {
                    "move_rt_sui_secp256r1_ecrecover"
                };
                let pk = self.runtime_bytes(rt, vec![sig.into(), msg.into(), hash.into()])?;
                fr.push(pk, Type::Vector(Box::new(Type::U8)));
            }
            "ecdsa_k1::decompress_pubkey" => {
                let (pk, _) = fr.pop()?;
                let out =
                    self.runtime_bytes("move_rt_sui_secp256k1_decompress_pubkey", vec![pk.into()])?;
                fr.push(out, Type::Vector(Box::new(Type::U8)));
            }
            "ecdsa_k1::secp256k1_verify" | "ecdsa_r1::secp256r1_verify" => {
                let (hash, _) = fr.pop()?;
                let (msg, _) = fr.pop()?;
                let (pk, _) = fr.pop()?;
                let (sig, _) = fr.pop()?;
                let rt = if name.starts_with("ecdsa_k1") {
                    "move_rt_sui_secp256k1_verify"
                } else {
                    "move_rt_sui_secp256r1_verify"
                };
                let ok =
                    self.runtime_bool(rt, &[sig.into(), pk.into(), msg.into(), hash.into()])?;
                fr.push(ok, Type::Bool);
            }
            "ed25519::ed25519_verify" => {
                let (msg, _) = fr.pop()?;
                let (pk, _) = fr.pop()?;
                let (sig, _) = fr.pop()?;
                let ok = self.runtime_bool(
                    "move_rt_sui_ed25519_verify",
                    &[sig.into(), pk.into(), msg.into()],
                )?;
                fr.push(ok, Type::Bool);
            }
            "hmac::hmac_sha3_256" => {
                let (msg, _) = fr.pop()?;
                let (key, _) = fr.pop()?;
                let out =
                    self.runtime_bytes("move_rt_sui_hmac_sha3_256", vec![key.into(), msg.into()])?;
                fr.push(out, Type::Vector(Box::new(Type::U8)));
            }

            // ---- types / address
            "types::is_one_time_witness" => {
                fr.pop()?;
                let is_otw = self
                    .ctx
                    .bool_type()
                    .const_int(self.is_one_time_witness(&t(0)) as u64, false);
                fr.push(is_otw, Type::Bool);
            }
            // addresses are held as their 32 bytes in memory order; as a u256 they read big-endian
            "address::to_u256" | "address::from_u256" => {
                let (v, _) = fr.pop()?;
                let bswap = inkwell::intrinsics::Intrinsic::find("llvm.bswap").unwrap();
                let decl = bswap
                    .get_declaration(&self.module, &[self.int_type(256).into()])
                    .unwrap();
                let swapped = self
                    .builder
                    .build_call(decl, &[v.into()], "")?
                    .try_as_basic_value()
                    .basic()
                    .unwrap();
                fr.push(
                    swapped,
                    if name.ends_with("to_u256") {
                        Type::U256
                    } else {
                        Type::Address
                    },
                );
            }
            "address::from_bytes" => {
                let (v, _) = fr.pop()?;
                let (data, len) = self.vec_parts(v.into_struct_value())?;
                let bad = self.builder.build_int_compare(
                    inkwell::IntPredicate::NE,
                    len,
                    i64t.const_int(32, false),
                    "",
                )?;
                // `sui::address::EAddressParseError` = 0
                let ok = self.ctx.append_basic_block(self.current_function(), "ok");
                let abort = self
                    .ctx
                    .append_basic_block(self.current_function(), "bad_address");
                self.builder.build_conditional_branch(bad, abort, ok)?;
                self.builder.position_at_end(abort);
                self.abort_in(i64t.const_zero(), f.id.module)?; // in `sui::address`
                self.builder.position_at_end(ok);
                let a = self.builder.build_load(self.int_type(256), data, "")?;
                fr.push(a, Type::Address);
            }

            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Sui's one-time-witness rule: a struct named after its module in upper case, with only
    /// the `drop` ability, no type parameters and a single `bool` field.
    fn is_one_time_witness(&self, ty: &Type) -> bool {
        let Type::Struct(id, args) = ty else {
            return false;
        };
        let cm = self.prog.module(id.module);
        let handle = cm.datatype_handle_at(cm.struct_def_at(id.def).struct_handle);
        let only_drop = handle.abilities
            == move_binary_format::file_format::AbilitySet::EMPTY
                | move_binary_format::file_format::Ability::Drop;
        let name = cm.identifier_at(handle.name).as_str();
        let fields = super::types::struct_fields(self.prog, *id, args).unwrap_or_default();
        args.is_empty()
            && only_drop
            && name == cm.name().as_str().to_uppercase()
            && fields == [Type::Bool]
    }

    fn clock_type(&self) -> Result<Type> {
        let id = self
            .prog
            .struct_by_name(
                &move_core_types::account_address::AccountAddress::TWO,
                "clock",
                "Clock",
            )
            .ok_or_else(|| anyhow::anyhow!("sui::clock::Clock not loaded"))?;
        Ok(Type::Struct(id, vec![]))
    }

    /// The ID-deriving runtime function: Sui's derivation, or its `--fast` variant.
    fn id_fn(&self, exact: &'static str) -> &'static str {
        match (self.options.fast, exact) {
            (false, _) => exact,
            (true, "move_rt_sui_fresh_id") => "move_rt_sui_fresh_id_fast",
            (true, _) => "move_rt_sui_df_hash_fast",
        }
    }

    /// Call a runtime function that writes an address to a trailing out-pointer.
    fn address_from_runtime(
        &self,
        name: &str,
        mut args: Vec<BasicMetadataValueEnum<'ctx>>,
    ) -> Result<IntValue<'ctx>> {
        let i256 = self.int_type(256);
        let out = self.builder.build_alloca(i256, "address")?;
        args.push(out.into());
        self.call_runtime(name, None, &args)?;
        Ok(self.builder.build_load(i256, out, "")?.into_int_value())
    }

    /// Record an effect (event / transfer / share / freeze) of a value of type `ty`, and
    /// remember the type so the program ABI can describe it.
    fn effect(
        &mut self,
        rt: &str,
        ty: &Type,
        value: BasicValueEnum<'ctx>,
        recipient: Option<BasicValueEnum<'ctx>>,
    ) -> Result<()> {
        // effects carry the canonical type name (defining IDs), as Sui's type tags do
        let (name, name_len) = self.const_bytes(ty.canonical(self.prog).as_bytes());
        let mut args: Vec<BasicMetadataValueEnum> = vec![
            self.type_desc(ty)?.into(),
            name.into(),
            self.ctx.i64_type().const_int(name_len, false).into(),
            self.spill(value)?.into(),
        ];
        if let Some(r) = recipient {
            args.push(self.spill(r)?.into());
        }
        self.call_runtime(rt, None, &args)?;
        if !self.effect_types.contains(ty) {
            self.effect_types.push(ty.clone());
        }
        if rt != "move_rt_sui_emit" && rt != "move_rt_sui_freeze" {
            self.note_state_type(ty); // transferred / shared objects persist in the state
        }
        Ok(())
    }
}
