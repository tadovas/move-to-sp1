//! Native functions: functions declared `native` in Move, implemented by the translator
//! (inline IR) or the runtime library.

use anyhow::{Result, bail};
use inkwell::{
    IntPredicate,
    values::{BasicMetadataValueEnum, BasicValueEnum, IntValue, StructValue},
};
use move_core_types::account_address::AccountAddress;

use super::{
    FnInst, Translator,
    function::Frame,
    types::{Type, struct_fields},
};

impl<'ctx> Translator<'ctx, '_> {
    pub(super) fn call_native(&mut self, fr: &mut Frame<'ctx>, f: &FnInst) -> Result<()> {
        let name = self.prog.function_name(f.id);
        let handled = match *self.prog.module_address(f.id.module) {
            AccountAddress::ONE => self.std_native(fr, f, &name)?,
            AccountAddress::TWO => self.sui_native(fr, f, &name)?,
            _ => false,
        };
        if !handled {
            self.missing_native(fr, f, name)?;
        }
        Ok(())
    }

    /// An unsupported native: compiled to an abort at location
    /// `move2sp1::unsupported::<module>::<function>` (so a program fails only if it actually
    /// calls it), and listed in the program ABI (`unsupported_natives`).
    fn missing_native(&mut self, fr: &mut Frame<'ctx>, f: &FnInst, name: String) -> Result<()> {
        let (loc, len) = self.const_bytes(format!("move2sp1::unsupported::{name}").as_bytes());
        self.missing_natives.insert(name);
        let (params, returns) = self.signature(f)?;
        fr.pop_n(params.len())?;
        let i64t = self.ctx.i64_type();
        self.call_runtime(
            "move_rt_abort",
            None,
            &[
                i64t.const_zero().into(),
                loc.into(),
                i64t.const_int(len, false).into(),
            ],
        )?;
        self.builder.build_unreachable()?;
        let dead = self
            .ctx
            .append_basic_block(self.current_function(), "missing_native");
        self.builder.position_at_end(dead);
        for ty in returns {
            let undef = self.llvm_type(&ty)?.const_zero();
            fr.push(undef, ty);
        }
        Ok(())
    }

    /// `std` (0x1) natives. Returns false if `name` is not one of them.
    fn std_native(&mut self, fr: &mut Frame<'ctx>, f: &FnInst, name: &str) -> Result<bool> {
        let t = |i: usize| f.type_args[i].clone();
        match name {
            "vector::empty" => {
                let v = self.vec_from_values(&t(0), &[])?;
                fr.push(v, Type::Vector(Box::new(t(0))));
            }
            "vector::length" => {
                let (r, _) = fr.pop()?;
                let len = self.vec_len(r.into_pointer_value())?;
                fr.push(len, Type::U64);
            }
            "vector::borrow" | "vector::borrow_mut" => {
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                let p = self.vec_elem_ptr(&t(0), r.into_pointer_value(), i.into_int_value())?;
                fr.push(p, Type::Ref(Box::new(t(0))));
            }
            "vector::push_back" => {
                let (v, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                self.vec_push(&t(0), r.into_pointer_value(), v)?;
            }
            "vector::pop_back" => {
                let (r, _) = fr.pop()?;
                let v = self.vec_pop(&t(0), r.into_pointer_value())?;
                fr.push(v, t(0));
            }
            "vector::destroy_empty" => {
                let (v, _) = fr.pop()?;
                self.vec_destroy_empty(v.into_struct_value())?;
            }
            "vector::swap" => {
                let (j, _) = fr.pop()?;
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                self.vec_swap(
                    &t(0),
                    r.into_pointer_value(),
                    i.into_int_value(),
                    j.into_int_value(),
                )?;
            }
            "vector::reverse" => {
                let (r, _) = fr.pop()?;
                self.vec_reverse(&t(0), r.into_pointer_value())?;
            }
            "vector::keep_range" => {
                let (j, _) = fr.pop()?;
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                self.vec_keep_range(
                    &t(0),
                    r.into_pointer_value(),
                    i.into_int_value(),
                    j.into_int_value(),
                )?;
            }
            "vector::copy_range" => {
                let (j, _) = fr.pop()?;
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                let v = self.vec_copy_range(
                    &t(0),
                    r.into_pointer_value(),
                    i.into_int_value(),
                    j.into_int_value(),
                )?;
                fr.push(v, Type::Vector(Box::new(t(0))));
            }
            "vector::replace_range" => {
                let (other, _) = fr.pop()?;
                let (j, _) = fr.pop()?;
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                let removed = self.vec_replace_range(
                    &t(0),
                    r.into_pointer_value(),
                    i.into_int_value(),
                    j.into_int_value(),
                    other.into_struct_value(),
                )?;
                fr.push(removed, Type::Vector(Box::new(t(0))));
            }

            // ---- implemented by the runtime (runtime/src/natives.rs)
            "bcs::to_bytes" => {
                let (r, _) = fr.pop()?;
                let v = self.runtime_bytes(
                    "move_rt_bcs_to_bytes",
                    vec![self.type_desc(&t(0))?.into(), r.into()],
                )?;
                fr.push(v, bytes_ty());
            }
            "debug::print" => {
                let (r, _) = fr.pop()?;
                self.call_runtime(
                    "move_rt_debug_print",
                    None,
                    &[self.type_desc(&t(0))?.into(), r.into()],
                )?;
            }
            "debug::print_stack_trace" => {}
            "unit_test::destroy" => {
                fr.pop()?; // test-only: drop any value (memory is never freed anyway)
            }
            // Resolved at compile time. `get` / `get_with_original_ids` are the natives of older
            // frameworks (before `with_defining_ids` / `with_original_ids`).
            "type_name::with_defining_ids"
            | "type_name::with_original_ids"
            | "type_name::get"
            | "type_name::get_with_original_ids" => {
                let (_, returns) = self.signature(f)?;
                let name = t(0).canonical_with(self.prog, !name.contains("original_ids"));
                let bytes = self.vec_from_bytes(name.as_bytes())?;
                let v = self.wrap_bytes(&returns[0], bytes.into())?;
                fr.push(v, returns[0].clone());
            }
            "type_name::defining_id" | "type_name::original_id" => {
                let Some(addr) = t(0).datatype_address(self.prog, name.ends_with("defining_id"))
                else {
                    bail!(
                        "{name}<{}>: not a struct or enum type",
                        t(0).display(self.prog)
                    );
                };
                let addr = addr.into_bytes();
                fr.push(self.const_int_le(256, &addr), Type::Address);
            }
            "hash::sha2_256" | "hash::sha3_256" => {
                let (v, _) = fr.pop()?;
                let rt = if name == "hash::sha2_256" {
                    "move_rt_sha2_256"
                } else {
                    "move_rt_sha3_256"
                };
                let digest = self.runtime_bytes(rt, vec![self.spill(v)?.into()])?;
                fr.push(digest, bytes_ty());
            }
            "string::internal_check_utf8" => {
                let (r, _) = fr.pop()?;
                let ok = self.runtime_bool("move_rt_utf8_check", &[r.into()])?;
                fr.push(ok, Type::Bool);
            }
            "string::internal_is_char_boundary" => {
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                let ok =
                    self.runtime_bool("move_rt_utf8_is_char_boundary", &[r.into(), i.into()])?;
                fr.push(ok, Type::Bool);
            }
            "string::internal_sub_string" => {
                let (j, _) = fr.pop()?;
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                let v = self.runtime_bytes(
                    "move_rt_utf8_sub_string",
                    vec![r.into(), i.into(), j.into()],
                )?;
                fr.push(v, bytes_ty());
            }
            "string::internal_index_of" => {
                let (needle, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                let i64t = self.ctx.i64_type().into();
                let idx = self.call_runtime(
                    "move_rt_utf8_index_of",
                    Some(i64t),
                    &[r.into(), needle.into()],
                )?;
                fr.push(idx.unwrap(), Type::U64);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Wrap a `vector<u8>` in single-field structs down to `ty` (e.g. `TypeName { name:
    /// ascii::String { bytes } }`).
    fn wrap_bytes(&self, ty: &Type, bytes: BasicValueEnum<'ctx>) -> Result<BasicValueEnum<'ctx>> {
        match ty {
            Type::Vector(_) => Ok(bytes),
            Type::Struct(id, args) => {
                let fields = struct_fields(self.prog, *id, args)?;
                let [inner] = fields.as_slice() else {
                    bail!("expected a single-field wrapper struct")
                };
                let v = self.wrap_bytes(inner, bytes)?;
                let st = self.llvm_type(ty)?.into_struct_type();
                Ok(self
                    .builder
                    .build_insert_value(st.get_undef(), v, 0, "")?
                    .into_struct_value()
                    .into())
            }
            other => bail!("cannot wrap bytes into {other:?}"),
        }
    }

    /// Call a runtime native that produces a `vector<u8>` through a trailing out-pointer.
    pub(super) fn runtime_bytes(
        &self,
        name: &str,
        mut args: Vec<BasicMetadataValueEnum<'ctx>>,
    ) -> Result<StructValue<'ctx>> {
        let out = self.builder.build_alloca(self.vector_type(), "out")?;
        args.push(out.into());
        self.call_runtime(name, None, &args)?;
        Ok(self
            .builder
            .build_load(self.vector_type(), out, "")?
            .into_struct_value())
    }

    /// Call a runtime native returning a Rust `bool` (0/1 in a register).
    pub(super) fn runtime_bool(
        &self,
        name: &str,
        args: &[BasicMetadataValueEnum<'ctx>],
    ) -> Result<IntValue<'ctx>> {
        let r = self
            .call_runtime(name, Some(self.ctx.i8_type().into()), args)?
            .unwrap();
        let zero = self.ctx.i8_type().const_zero();
        Ok(self
            .builder
            .build_int_compare(IntPredicate::NE, r.into_int_value(), zero, "")?)
    }
}

fn bytes_ty() -> Type {
    Type::Vector(Box::new(Type::U8))
}
