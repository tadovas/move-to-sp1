//! Type-directed value operations: deep copy, structural equality and constants.
//!
//! Copy and equality of vectors / structs containing vectors are emitted as one internal
//! helper function per type (`move.copy.<T>`, `move.eq.<T>`), generated on first use.

use anyhow::{Result, bail, ensure};
use inkwell::{
    IntPredicate,
    types::BasicType,
    values::{BasicValueEnum, IntValue},
};

use super::{
    Translator,
    types::{Type, struct_fields, variant_fields},
};

impl<'ctx> Translator<'ctx, '_> {
    /// Whether copying `ty` must duplicate heap memory (it contains a vector).
    fn has_heap(&self, ty: &Type) -> Result<bool> {
        Ok(match ty {
            Type::Vector(_) => true,
            Type::Struct(id, args) => {
                let mut any = false;
                for f in struct_fields(self.prog, *id, args)? {
                    any |= self.has_heap(&f)?;
                }
                any
            }
            Type::Enum(id, args) => {
                let mut any = false;
                for f in variant_fields(self.prog, *id, args)?.iter().flatten() {
                    any |= self.has_heap(f)?;
                }
                any
            }
            _ => false,
        })
    }

    /// Move `copy`: vectors are values, so a copy gets its own buffer.
    pub(super) fn copy_value(
        &mut self,
        ty: &Type,
        v: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>> {
        if !self.has_heap(ty)? {
            return Ok(v);
        }
        let lty = self.llvm_type(ty)?;
        let name = format!("move.copy.{}", ty.display(self.prog));
        let helper = self.helper(&name, lty.fn_type(&[lty.into()], false), |t, f| {
            let src = f.get_first_param().unwrap();
            let out = t.copy_body(ty, src)?;
            t.builder.build_return(Some(&out))?;
            Ok(())
        })?;
        let call = self.builder.build_call(helper, &[v.into()], "copy")?;
        Ok(call.try_as_basic_value().basic().unwrap())
    }

    fn copy_body(&mut self, ty: &Type, src: BasicValueEnum<'ctx>) -> Result<BasicValueEnum<'ctx>> {
        match ty {
            Type::Vector(elem) => {
                let (data, len) = self.vec_parts(src.into_struct_value())?;
                let bytes = self.builder.build_int_mul(len, self.size_of(elem)?, "")?;
                let new = self.alloc(bytes)?;
                if self.has_heap(elem)? {
                    let ety = self.llvm_type(elem)?;
                    self.counted_loop(len, |t, i| {
                        let from = t.elem_ptr_unchecked(elem, data, i)?;
                        let to = t.elem_ptr_unchecked(elem, new, i)?;
                        let e = t.builder.build_load(ety, from, "")?;
                        let e = t.copy_value(elem, e)?;
                        t.builder.build_store(to, e)?;
                        Ok(())
                    })?;
                } else {
                    self.builder
                        .build_memcpy(new, 16, data, 16, bytes)
                        .map_err(anyhow::Error::msg)?;
                }
                Ok(self.vec_from_parts(new, len)?.into())
            }
            Type::Struct(id, args) => {
                let mut agg = src.into_struct_value();
                for (i, fty) in struct_fields(self.prog, *id, args)?.iter().enumerate() {
                    let f = self.builder.build_extract_value(agg, i as u32, "")?;
                    let f = self.copy_value(fty, f)?;
                    agg = self
                        .builder
                        .build_insert_value(agg, f, i as u32, "")?
                        .into_struct_value();
                }
                Ok(agg.into())
            }
            Type::Enum(..) => {
                // copy the whole value, then deep-copy the active variant's fields in place
                let lty = self.llvm_type(ty)?;
                let tmp = self.builder.build_alloca(lty, "enum")?;
                self.builder.build_store(tmp, src)?;
                self.for_each_variant(ty, tmp, |t, _, fields| {
                    for (p, fty) in fields {
                        let v = t.builder.build_load(t.llvm_type(&fty)?, p, "")?;
                        let v = t.copy_value(&fty, v)?;
                        t.builder.build_store(p, v)?;
                    }
                    Ok(())
                })?;
                Ok(self.builder.build_load(lty, tmp, "")?)
            }
            _ => Ok(src),
        }
    }

    /// Move `==`: structural equality; references compare the values they point to.
    pub(super) fn equals(
        &mut self,
        ty: &Type,
        l: BasicValueEnum<'ctx>,
        r: BasicValueEnum<'ctx>,
    ) -> Result<IntValue<'ctx>> {
        match ty {
            Type::Ref(inner) => {
                let lty = self.llvm_type(inner)?;
                let l = self.builder.build_load(lty, l.into_pointer_value(), "")?;
                let r = self.builder.build_load(lty, r.into_pointer_value(), "")?;
                self.equals(inner, l, r)
            }
            Type::Vector(_) | Type::Struct(..) | Type::Enum(..) => {
                let lty = self.llvm_type(ty)?;
                let name = format!("move.eq.{}", ty.display(self.prog));
                let fty = self
                    .ctx
                    .bool_type()
                    .fn_type(&[lty.into(), lty.into()], false);
                let helper = self.helper(&name, fty, |t, f| {
                    let (a, b) = (f.get_nth_param(0).unwrap(), f.get_nth_param(1).unwrap());
                    t.eq_body(ty, a, b)
                })?;
                let call = self
                    .builder
                    .build_call(helper, &[l.into(), r.into()], "eq")?;
                Ok(call.try_as_basic_value().basic().unwrap().into_int_value())
            }
            _ => Ok(self.builder.build_int_compare(
                IntPredicate::EQ,
                l.into_int_value(),
                r.into_int_value(),
                "",
            )?),
        }
    }

    /// Body of `move.eq.<T>`: returns early on the first difference.
    fn eq_body(
        &mut self,
        ty: &Type,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
    ) -> Result<()> {
        let f = self.current_function();
        let (yes, no) = (
            self.ctx.bool_type().const_int(1, false),
            self.ctx.bool_type().const_zero(),
        );
        let differ = self.ctx.append_basic_block(f, "differ");
        match ty {
            Type::Vector(elem) => {
                let (da, la) = self.vec_parts(a.into_struct_value())?;
                let (db, lb) = self.vec_parts(b.into_struct_value())?;
                let same_len = self
                    .builder
                    .build_int_compare(IntPredicate::EQ, la, lb, "")?;
                let cont = self.ctx.append_basic_block(f, "same_len");
                self.builder
                    .build_conditional_branch(same_len, cont, differ)?;
                self.builder.position_at_end(cont);
                let ety = self.llvm_type(elem)?;
                self.counted_loop(la, |t, i| {
                    let x = t
                        .builder
                        .build_load(ety, t.elem_ptr_unchecked(elem, da, i)?, "")?;
                    let y = t
                        .builder
                        .build_load(ety, t.elem_ptr_unchecked(elem, db, i)?, "")?;
                    let eq = t.equals(elem, x, y)?;
                    let next = t.ctx.append_basic_block(f, "next");
                    t.builder.build_conditional_branch(eq, next, differ)?;
                    t.builder.position_at_end(next);
                    Ok(())
                })?;
            }
            Type::Struct(id, args) => {
                for (i, fty) in struct_fields(self.prog, *id, args)?.iter().enumerate() {
                    let x =
                        self.builder
                            .build_extract_value(a.into_struct_value(), i as u32, "")?;
                    let y =
                        self.builder
                            .build_extract_value(b.into_struct_value(), i as u32, "")?;
                    let eq = self.equals(fty, x, y)?;
                    let next = self.ctx.append_basic_block(f, "next");
                    self.builder.build_conditional_branch(eq, next, differ)?;
                    self.builder.position_at_end(next);
                }
            }
            Type::Enum(..) => {
                let lty = self.llvm_type(ty)?;
                let (pa, pb) = (
                    self.builder.build_alloca(lty, "a")?,
                    self.builder.build_alloca(lty, "b")?,
                );
                self.builder.build_store(pa, a)?;
                self.builder.build_store(pb, b)?;
                let same_tag = self.builder.build_int_compare(
                    IntPredicate::EQ,
                    self.load_tag(ty, pa)?,
                    self.load_tag(ty, pb)?,
                    "",
                )?;
                let cont = self.ctx.append_basic_block(f, "same_tag");
                self.builder
                    .build_conditional_branch(same_tag, cont, differ)?;
                self.builder.position_at_end(cont);
                self.for_each_variant(ty, pa, |t, tag, fields_a| {
                    let fields_b = t.field_ptrs(ty, pb, tag)?;
                    for ((x, fty), (y, _)) in fields_a.into_iter().zip(fields_b) {
                        let lty = t.llvm_type(&fty)?;
                        let (x, y) = (
                            t.builder.build_load(lty, x, "")?,
                            t.builder.build_load(lty, y, "")?,
                        );
                        let eq = t.equals(&fty, x, y)?;
                        let next = t.ctx.append_basic_block(f, "next");
                        t.builder.build_conditional_branch(eq, next, differ)?;
                        t.builder.position_at_end(next);
                    }
                    Ok(())
                })?;
            }
            other => bail!("eq_body on {other:?}"),
        }
        self.builder.build_return(Some(&yes))?;
        self.builder.position_at_end(differ);
        self.builder.build_return(Some(&no))?;
        Ok(())
    }

    /// A constant-pool value (BCS encoded): scalars, addresses and vectors of those.
    pub(super) fn constant(&mut self, ty: &Type, bcs: &[u8]) -> Result<BasicValueEnum<'ctx>> {
        let mut r = Reader(bcs);
        let v = self.read_constant(ty, &mut r)?;
        ensure!(r.0.is_empty(), "trailing bytes in constant");
        Ok(v)
    }

    fn read_constant(&mut self, ty: &Type, r: &mut Reader) -> Result<BasicValueEnum<'ctx>> {
        Ok(match ty {
            // addresses keep their 32 raw bytes in memory order
            Type::Bool
            | Type::U8
            | Type::U16
            | Type::U32
            | Type::U64
            | Type::U128
            | Type::U256
            | Type::Address => {
                let bits = ty.int_bits().unwrap();
                let bytes = r.take(bits.div_ceil(8) as usize)?;
                self.const_int_le(bits, bytes).into()
            }
            Type::Vector(elem) => {
                let n = r.uleb128()?;
                let elems = (0..n)
                    .map(|_| self.read_constant(elem, r))
                    .collect::<Result<Vec<_>>>()?;
                self.vec_from_values(elem, &elems)?.into()
            }
            other => bail!("constant of type {other:?} not supported"),
        })
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.0.len() >= n, "truncated constant");
        let (h, t) = self.0.split_at(n);
        self.0 = t;
        Ok(h)
    }
    fn uleb128(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.take(1)?[0];
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        bail!("bad uleb128")
    }
}
