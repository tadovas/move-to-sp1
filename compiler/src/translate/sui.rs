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
                let a = self.address_from_runtime("move_rt_sui_fresh_id", vec![])?;
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
                let args = vec![
                    self.spill(parent)?.into(),
                    self.type_desc(&t(0))?.into(),
                    self.spill(key)?.into(),
                ];
                let id = self.address_from_runtime("move_rt_sui_df_hash", args)?;
                fr.push(id, Type::Address);
            }
            "dynamic_field::add_child_object" => {
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

            _ => return Ok(false),
        }
        Ok(true)
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
        let mut args: Vec<BasicMetadataValueEnum> =
            vec![self.type_desc(ty)?.into(), self.spill(value)?.into()];
        if let Some(r) = recipient {
            args.push(self.spill(r)?.into());
        }
        self.call_runtime(rt, None, &args)?;
        if !self.effect_types.contains(ty) {
            self.effect_types.push(ty.clone());
        }
        Ok(())
    }
}
