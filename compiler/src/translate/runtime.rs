//! The interface between generated code and the runtime library (`runtime/src/lib.rs`,
//! `runtime/src/natives.rs`). All `move_rt_*` calls go through [`Translator::call_runtime`],
//! which declares the symbol from the call's argument types on first use.

use anyhow::Result;
use inkwell::{
    AddressSpace,
    attributes::{Attribute, AttributeLoc},
    types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, PointerType},
    values::{BasicMetadataValueEnum, BasicValueEnum, IntValue, PointerValue},
};

use super::Translator;

/// Move VM status codes (`move_core_types::vm_status::StatusCode`) for runtime failures
/// that are not user `abort`s.
#[derive(Clone, Copy)]
#[repr(u64)]
pub enum VmError {
    Arithmetic = 4017,
    VectorOperation = 4018,
    VariantTagMismatch = 4030,
}

/// Runtime functions that never return.
const NORETURN: &[&str] = &["move_rt_abort", "move_rt_vm_error"];

impl<'ctx> Translator<'ctx, '_> {
    pub(super) fn ptr_type(&self) -> PointerType<'ctx> {
        self.ctx.ptr_type(AddressSpace::default())
    }

    /// Call runtime function `name` (declared on first use from the argument types).
    pub(super) fn call_runtime(
        &self,
        name: &str,
        ret: Option<BasicTypeEnum<'ctx>>,
        args: &[BasicMetadataValueEnum<'ctx>],
    ) -> Result<Option<BasicValueEnum<'ctx>>> {
        let f = self.module.get_function(name).unwrap_or_else(|| {
            let params: Vec<BasicMetadataTypeEnum> = args
                .iter()
                .map(|a| {
                    BasicValueEnum::try_from(*a)
                        .expect("basic arg")
                        .get_type()
                        .into()
                })
                .collect();
            let ty = match ret {
                Some(r) => r.fn_type(&params, false),
                None => self.ctx.void_type().fn_type(&params, false),
            };
            let f = self.module.add_function(name, ty, None);
            let mut attrs = vec!["nounwind"];
            if NORETURN.contains(&name) {
                attrs.extend(["noreturn", "cold"]);
            }
            for a in attrs {
                let kind = Attribute::get_named_enum_kind_id(a);
                f.add_attribute(
                    AttributeLoc::Function,
                    self.ctx.create_enum_attribute(kind, 0),
                );
            }
            f
        });
        let call = self.builder.build_call(f, args, "")?;
        Ok(call.try_as_basic_value().basic())
    }

    /// Heap memory (16-byte aligned, never freed).
    pub(super) fn alloc(&self, size: IntValue<'ctx>) -> Result<PointerValue<'ctx>> {
        let align = self.ctx.i64_type().const_int(16, false);
        let p = self.call_runtime(
            "move_rt_alloc",
            Some(self.ptr_type().into()),
            &[size.into(), align.into()],
        )?;
        Ok(p.unwrap().into_pointer_value())
    }

    /// Move `abort code`.
    pub(super) fn abort(&self, code: IntValue<'ctx>) -> Result<()> {
        self.call_runtime("move_rt_abort", None, &[code.into()])?;
        self.builder.build_unreachable()?;
        Ok(())
    }

    /// Branch to `move_rt_vm_error(err)` when `fail` holds; continue in a fresh block otherwise.
    pub(super) fn fail_if(&self, fail: IntValue<'ctx>, err: VmError) -> Result<()> {
        let f = self.current_function();
        let ok = self.ctx.append_basic_block(f, "ok");
        let bad = self.ctx.append_basic_block(f, "vm_error");
        self.builder.build_conditional_branch(fail, bad, ok)?;
        self.builder.position_at_end(bad);
        let code = self.ctx.i64_type().const_int(err as u64, false);
        self.call_runtime("move_rt_vm_error", None, &[code.into()])?;
        self.builder.build_unreachable()?;
        self.builder.position_at_end(ok);
        Ok(())
    }

    /// Spill a value to a stack slot and return its address (runtime calls take pointers).
    pub(super) fn spill(&self, v: BasicValueEnum<'ctx>) -> Result<PointerValue<'ctx>> {
        let slot = self.builder.build_alloca(v.get_type(), "spill")?;
        self.builder.build_store(slot, v)?;
        Ok(slot)
    }
}
