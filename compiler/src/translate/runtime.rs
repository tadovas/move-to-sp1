//! The interface between generated code and the runtime library (`runtime/src/lib.rs`).
//!
//! Every `move_rt_*` symbol the translator may call is declared here, in one place.

use inkwell::{
    AddressSpace,
    attributes::{Attribute, AttributeLoc},
    values::{FunctionValue, IntValue, PointerValue},
};

use super::Translator;

/// Move VM status codes (`move_core_types::vm_status::StatusCode`) for runtime failures
/// that are not user `abort`s.
#[derive(Clone, Copy)]
#[repr(u64)]
pub enum VmError {
    Arithmetic = 4017,
    VectorOperation = 4018,
}

impl<'ctx> Translator<'ctx, '_> {
    fn runtime_fn(
        &self,
        name: &str,
        params: &[inkwell::types::BasicMetadataTypeEnum<'ctx>],
        ret_ptr: bool,
        noreturn: bool,
    ) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function(name) {
            return f;
        }
        let ty = if ret_ptr {
            self.ptr_type().fn_type(params, false)
        } else {
            self.ctx.void_type().fn_type(params, false)
        };
        let f = self.module.add_function(name, ty, None);
        let attr = |n: &str| {
            let kind = Attribute::get_named_enum_kind_id(n);
            f.add_attribute(
                AttributeLoc::Function,
                self.ctx.create_enum_attribute(kind, 0),
            );
        };
        attr("nounwind");
        if noreturn {
            attr("noreturn");
            attr("cold");
        }
        f
    }

    pub(super) fn ptr_type(&self) -> inkwell::types::PointerType<'ctx> {
        self.ctx.ptr_type(AddressSpace::default())
    }

    /// `move_rt_abort(code: u64) -> !` — Move `abort code`.
    pub(super) fn rt_abort(&self) -> FunctionValue<'ctx> {
        self.runtime_fn("move_rt_abort", &[self.ctx.i64_type().into()], false, true)
    }

    /// `move_rt_vm_error(status: u64) -> !` — VM-level failure (overflow, bad index, ...).
    pub(super) fn rt_vm_error(&self) -> FunctionValue<'ctx> {
        self.runtime_fn(
            "move_rt_vm_error",
            &[self.ctx.i64_type().into()],
            false,
            true,
        )
    }

    /// `move_rt_alloc(size: u64, align: u64) -> *mut u8` — never freed (bump allocator).
    pub(super) fn rt_alloc(&self) -> FunctionValue<'ctx> {
        let i64t = self.ctx.i64_type();
        self.runtime_fn("move_rt_alloc", &[i64t.into(), i64t.into()], true, false)
    }

    /// `move_rt_read_arg(dst, size)` — next SP1 stdin buffer, exactly `size` bytes.
    pub(super) fn rt_read_arg(&self) -> FunctionValue<'ctx> {
        self.runtime_fn(
            "move_rt_read_arg",
            &[self.ptr_type().into(), self.ctx.i64_type().into()],
            false,
            false,
        )
    }

    /// `move_rt_commit(src, len)` — append to the public values.
    pub(super) fn rt_commit(&self) -> FunctionValue<'ctx> {
        self.runtime_fn(
            "move_rt_commit",
            &[self.ptr_type().into(), self.ctx.i64_type().into()],
            false,
            false,
        )
    }

    /// Emit a call to `move_rt_alloc`; all heap memory is 16-byte aligned.
    pub(super) fn alloc(&self, size: IntValue<'ctx>) -> anyhow::Result<PointerValue<'ctx>> {
        let align = self.ctx.i64_type().const_int(16, false);
        let call =
            self.builder
                .build_call(self.rt_alloc(), &[size.into(), align.into()], "heap")?;
        Ok(call
            .try_as_basic_value()
            .basic()
            .unwrap()
            .into_pointer_value())
    }

    /// Branch to `move_rt_vm_error(err)` when `fail` holds; continue in a fresh block otherwise.
    pub(super) fn fail_if(&self, fail: IntValue<'ctx>, err: VmError) -> anyhow::Result<()> {
        let f = self.current_function();
        let ok = self.ctx.append_basic_block(f, "ok");
        let bad = self.ctx.append_basic_block(f, "vm_error");
        self.builder.build_conditional_branch(fail, bad, ok)?;
        self.builder.position_at_end(bad);
        let code = self.ctx.i64_type().const_int(err as u64, false);
        self.builder
            .build_call(self.rt_vm_error(), &[code.into()], "")?;
        self.builder.build_unreachable()?;
        self.builder.position_at_end(ok);
        Ok(())
    }
}
