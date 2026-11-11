//! Vectors.
//!
//! A `vector<T>` value is a header `{ data: ptr, len: i64, cap: i64 }`, with elements stored
//! contiguously in heap memory. Headers are values: moving a vector moves the header,
//! copying one deep-copies the buffer (`value.rs`). Operations on `&vector<T>` /
//! `&mut vector<T>` receive a pointer to the header. Memory is never freed (the SP1 heap is a
//! bump allocator); Move's ownership rules make the never-shared buffers safe to abandon.
//!
//! Out-of-bounds accesses, popping an empty vector and mismatched unpacks abort with
//! `VECTOR_OPERATION_ERROR`, like the Move VM.

use anyhow::Result;
use inkwell::{
    IntPredicate,
    types::StructType,
    values::{BasicValueEnum, IntValue, PointerValue, StructValue},
};

use super::{Translator, runtime::VmError, types::Type};

const DATA: u32 = 0;
const LEN: u32 = 1;
const CAP: u32 = 2;
const MIN_CAP: u64 = 4;

impl<'ctx> Translator<'ctx, '_> {
    pub(super) fn vector_type(&self) -> StructType<'ctx> {
        if let Some(t) = self.module.get_struct_type("move.vector") {
            return t;
        }
        let t = self.ctx.opaque_struct_type("move.vector");
        let i64t = self.ctx.i64_type();
        t.set_body(&[self.ptr_type().into(), i64t.into(), i64t.into()], false);
        t
    }

    fn header(
        &self,
        data: PointerValue<'ctx>,
        len: IntValue<'ctx>,
        cap: IntValue<'ctx>,
    ) -> Result<StructValue<'ctx>> {
        let b = &self.builder;
        let mut v = self.vector_type().get_undef();
        v = b.build_insert_value(v, data, DATA, "")?.into_struct_value();
        v = b.build_insert_value(v, len, LEN, "")?.into_struct_value();
        v = b.build_insert_value(v, cap, CAP, "")?.into_struct_value();
        Ok(v)
    }

    fn load_field(&self, vec: PointerValue<'ctx>, field: u32) -> Result<BasicValueEnum<'ctx>> {
        let ty = if field == DATA {
            self.ptr_type().into()
        } else {
            self.ctx.i64_type().into()
        };
        let p = self
            .builder
            .build_struct_gep(self.vector_type(), vec, field, "")?;
        Ok(self
            .builder
            .build_load::<inkwell::types::BasicTypeEnum>(ty, p, "")?)
    }

    fn store_field(
        &self,
        vec: PointerValue<'ctx>,
        field: u32,
        v: impl Into<BasicValueEnum<'ctx>>,
    ) -> Result<()> {
        let p = self
            .builder
            .build_struct_gep(self.vector_type(), vec, field, "")?;
        self.builder.build_store(p, v.into())?;
        Ok(())
    }

    fn data_ptr(&self, vec: PointerValue<'ctx>) -> Result<PointerValue<'ctx>> {
        Ok(self.load_field(vec, DATA)?.into_pointer_value())
    }

    /// `&data[i]` without bounds check.
    pub(super) fn elem_ptr_unchecked(
        &self,
        elem: &Type,
        data: PointerValue<'ctx>,
        i: IntValue<'ctx>,
    ) -> Result<PointerValue<'ctx>> {
        let ty = self.llvm_type(elem)?;
        Ok(unsafe { self.builder.build_in_bounds_gep(ty, data, &[i], "elem")? })
    }

    /// An empty vector with room for `cap` elements.
    pub(super) fn vec_with_capacity(
        &self,
        elem: &Type,
        cap: IntValue<'ctx>,
    ) -> Result<StructValue<'ctx>> {
        let size = self.builder.build_int_mul(cap, self.size_of(elem)?, "")?;
        let data = self.alloc(size)?;
        self.header(data, self.ctx.i64_type().const_zero(), cap)
    }

    /// `VecPack`: a vector holding `elems`.
    pub(super) fn vec_from_values(
        &self,
        elem: &Type,
        elems: &[BasicValueEnum<'ctx>],
    ) -> Result<StructValue<'ctx>> {
        let i64t = self.ctx.i64_type();
        let n = i64t.const_int(elems.len() as u64, false);
        let cap = i64t.const_int((elems.len() as u64).max(MIN_CAP), false);
        let v = self.vec_with_capacity(elem, cap)?;
        let data = self
            .builder
            .build_extract_value(v, DATA, "")?
            .into_pointer_value();
        for (i, e) in elems.iter().enumerate() {
            let p = self.elem_ptr_unchecked(elem, data, i64t.const_int(i as u64, false))?;
            self.builder.build_store(p, *e)?;
        }
        Ok(self
            .builder
            .build_insert_value(v, n, LEN, "")?
            .into_struct_value())
    }

    /// A `vector<u8>` holding `bytes`: one `memcpy` from a read-only constant.
    pub(super) fn vec_from_bytes(&self, bytes: &[u8]) -> Result<StructValue<'ctx>> {
        let n = self.ctx.i64_type().const_int(bytes.len() as u64, false);
        let data = self.alloc(n)?;
        if !bytes.is_empty() {
            let init = self.ctx.const_string(bytes, false);
            let g = self.module.add_global(init.get_type(), None, ".bytes");
            g.set_initializer(&init);
            g.set_constant(true);
            g.set_linkage(inkwell::module::Linkage::Private);
            g.set_unnamed_addr(true);
            self.builder
                .build_memcpy(data, 16, g.as_pointer_value(), 1, n)
                .map_err(anyhow::Error::msg)?;
        }
        self.vec_from_parts(data, n)
    }

    /// `VecUnpack`: aborts unless the vector has exactly `n` elements.
    pub(super) fn vec_unpack(
        &self,
        elem: &Type,
        v: StructValue<'ctx>,
        n: u64,
    ) -> Result<Vec<BasicValueEnum<'ctx>>> {
        let len = self
            .builder
            .build_extract_value(v, LEN, "")?
            .into_int_value();
        let want = self.ctx.i64_type().const_int(n, false);
        let bad = self
            .builder
            .build_int_compare(IntPredicate::NE, len, want, "")?;
        self.fail_if(bad, VmError::VectorOperation)?;
        let data = self
            .builder
            .build_extract_value(v, DATA, "")?
            .into_pointer_value();
        let ty = self.llvm_type(elem)?;
        (0..n)
            .map(|i| {
                let p =
                    self.elem_ptr_unchecked(elem, data, self.ctx.i64_type().const_int(i, false))?;
                Ok(self.builder.build_load(ty, p, "")?)
            })
            .collect()
    }

    pub(super) fn vec_len(&self, vec: PointerValue<'ctx>) -> Result<IntValue<'ctx>> {
        Ok(self.load_field(vec, LEN)?.into_int_value())
    }

    /// `&v[i]` / `&mut v[i]`, bounds checked.
    pub(super) fn vec_elem_ptr(
        &self,
        elem: &Type,
        vec: PointerValue<'ctx>,
        i: IntValue<'ctx>,
    ) -> Result<PointerValue<'ctx>> {
        self.bounds_check(vec, i)?;
        let data = self.data_ptr(vec)?;
        self.elem_ptr_unchecked(elem, data, i)
    }

    fn bounds_check(&self, vec: PointerValue<'ctx>, i: IntValue<'ctx>) -> Result<()> {
        let len = self.vec_len(vec)?;
        let oob = self
            .builder
            .build_int_compare(IntPredicate::UGE, i, len, "")?;
        self.fail_if(oob, VmError::VectorOperation)
    }

    pub(super) fn vec_push(
        &self,
        elem: &Type,
        vec: PointerValue<'ctx>,
        value: BasicValueEnum<'ctx>,
    ) -> Result<()> {
        let i64t = self.ctx.i64_type();
        let len = self.vec_len(vec)?;
        let cap = self.load_field(vec, CAP)?.into_int_value();

        // grow to max(MIN_CAP, 2 * cap) when full
        let f = self.current_function();
        let grow = self.ctx.append_basic_block(f, "vec.grow");
        let store = self.ctx.append_basic_block(f, "vec.push");
        let full = self
            .builder
            .build_int_compare(IntPredicate::UGE, len, cap, "")?;
        self.builder.build_conditional_branch(full, grow, store)?;

        self.builder.position_at_end(grow);
        let doubled = self
            .builder
            .build_int_mul(cap, i64t.const_int(2, false), "")?;
        let small = self.builder.build_int_compare(
            IntPredicate::ULT,
            doubled,
            i64t.const_int(MIN_CAP, false),
            "",
        )?;
        let new_cap = self
            .builder
            .build_select(small, i64t.const_int(MIN_CAP, false), doubled, "")?
            .into_int_value();
        let esize = self.size_of(elem)?;
        let new_data = self.alloc(self.builder.build_int_mul(new_cap, esize, "")?)?;
        let old_data = self.data_ptr(vec)?;
        let used = self.builder.build_int_mul(len, esize, "")?;
        self.builder
            .build_memcpy(new_data, 16, old_data, 16, used)
            .map_err(anyhow::Error::msg)?;
        self.store_field(vec, DATA, new_data)?;
        self.store_field(vec, CAP, new_cap)?;
        self.builder.build_unconditional_branch(store)?;

        self.builder.position_at_end(store);
        let data = self.data_ptr(vec)?;
        let p = self.elem_ptr_unchecked(elem, data, len)?;
        self.builder.build_store(p, value)?;
        let len1 = self
            .builder
            .build_int_add(len, i64t.const_int(1, false), "")?;
        self.store_field(vec, LEN, len1)
    }

    pub(super) fn vec_pop(
        &self,
        elem: &Type,
        vec: PointerValue<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>> {
        let len = self.vec_len(vec)?;
        let zero = self.ctx.i64_type().const_zero();
        let empty = self
            .builder
            .build_int_compare(IntPredicate::EQ, len, zero, "")?;
        self.fail_if(empty, VmError::VectorOperation)?;
        let last = self
            .builder
            .build_int_sub(len, self.ctx.i64_type().const_int(1, false), "")?;
        let data = self.data_ptr(vec)?;
        let p = self.elem_ptr_unchecked(elem, data, last)?;
        let v = self.builder.build_load(self.llvm_type(elem)?, p, "")?;
        self.store_field(vec, LEN, last)?;
        Ok(v)
    }

    pub(super) fn vec_swap(
        &self,
        elem: &Type,
        vec: PointerValue<'ctx>,
        i: IntValue<'ctx>,
        j: IntValue<'ctx>,
    ) -> Result<()> {
        let pi = self.vec_elem_ptr(elem, vec, i)?;
        let pj = self.vec_elem_ptr(elem, vec, j)?;
        let ty = self.llvm_type(elem)?;
        let vi = self.builder.build_load(ty, pi, "")?;
        let vj = self.builder.build_load(ty, pj, "")?;
        self.builder.build_store(pi, vj)?;
        self.builder.build_store(pj, vi)?;
        Ok(())
    }

    /// `vector::destroy_empty`: aborts unless empty.
    pub(super) fn vec_destroy_empty(&self, v: StructValue<'ctx>) -> Result<()> {
        let len = self
            .builder
            .build_extract_value(v, LEN, "")?
            .into_int_value();
        let nonempty = self.builder.build_int_compare(
            IntPredicate::NE,
            len,
            self.ctx.i64_type().const_zero(),
            "",
        )?;
        self.fail_if(nonempty, VmError::VectorOperation)
    }

    /// Fields of a vector *value* (header held in SSA).
    pub(super) fn vec_parts(
        &self,
        v: StructValue<'ctx>,
    ) -> Result<(PointerValue<'ctx>, IntValue<'ctx>)> {
        let data = self
            .builder
            .build_extract_value(v, DATA, "")?
            .into_pointer_value();
        let len = self
            .builder
            .build_extract_value(v, LEN, "")?
            .into_int_value();
        Ok((data, len))
    }

    /// A vector value with the given buffer, `len == cap`.
    pub(super) fn vec_from_parts(
        &self,
        data: PointerValue<'ctx>,
        len: IntValue<'ctx>,
    ) -> Result<StructValue<'ctx>> {
        self.header(data, len, len)
    }

    // ---- range natives (`std::vector`): a range [i, j) is valid iff i <= j <= len

    fn range_check(&self, i: IntValue<'ctx>, j: IntValue<'ctx>, len: IntValue<'ctx>) -> Result<()> {
        let b = &self.builder;
        let bad_ij = b.build_int_compare(IntPredicate::UGT, i, j, "")?;
        let bad_j = b.build_int_compare(IntPredicate::UGT, j, len, "")?;
        let bad = b.build_or(bad_ij, bad_j, "")?;
        self.fail_if(bad, VmError::VectorOperation)
    }

    /// `memcpy(dst, src + from * esize, n * esize)` (moves elements; no deep copy).
    fn copy_elems(
        &self,
        elem: &Type,
        dst: PointerValue<'ctx>,
        src: PointerValue<'ctx>,
        from: IntValue<'ctx>,
        n: IntValue<'ctx>,
    ) -> Result<()> {
        let src = self.elem_ptr_unchecked(elem, src, from)?;
        let bytes = self.builder.build_int_mul(n, self.size_of(elem)?, "")?;
        self.builder
            .build_memmove(dst, 1, src, 1, bytes)
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }

    /// `vector::reverse`
    pub(super) fn vec_reverse(&mut self, elem: &Type, vec: PointerValue<'ctx>) -> Result<()> {
        let i64t = self.ctx.i64_type();
        let len = self.vec_len(vec)?;
        let half = self
            .builder
            .build_int_unsigned_div(len, i64t.const_int(2, false), "")?;
        let last = self
            .builder
            .build_int_sub(len, i64t.const_int(1, false), "")?;
        let data = self.data_ptr(vec)?;
        let ty = self.llvm_type(elem)?;
        self.counted_loop(half, |t, i| {
            let j = t.builder.build_int_sub(last, i, "")?;
            let (pi, pj) = (
                t.elem_ptr_unchecked(elem, data, i)?,
                t.elem_ptr_unchecked(elem, data, j)?,
            );
            let (vi, vj) = (
                t.builder.build_load(ty, pi, "")?,
                t.builder.build_load(ty, pj, "")?,
            );
            t.builder.build_store(pi, vj)?;
            t.builder.build_store(pj, vi)?;
            Ok(())
        })
    }

    /// `vector::keep_range`: keep [i, j), drop the rest.
    pub(super) fn vec_keep_range(
        &self,
        elem: &Type,
        vec: PointerValue<'ctx>,
        i: IntValue<'ctx>,
        j: IntValue<'ctx>,
    ) -> Result<()> {
        self.range_check(i, j, self.vec_len(vec)?)?;
        let data = self.data_ptr(vec)?;
        let n = self.builder.build_int_sub(j, i, "")?;
        self.copy_elems(elem, data, data, i, n)?;
        self.store_field(vec, LEN, n)
    }

    /// `vector::copy_range`: a (deep) copy of [i, j).
    pub(super) fn vec_copy_range(
        &mut self,
        elem: &Type,
        vec: PointerValue<'ctx>,
        i: IntValue<'ctx>,
        j: IntValue<'ctx>,
    ) -> Result<StructValue<'ctx>> {
        self.range_check(i, j, self.vec_len(vec)?)?;
        let n = self.builder.build_int_sub(j, i, "")?;
        let new = self.alloc(self.builder.build_int_mul(n, self.size_of(elem)?, "")?)?;
        self.copy_elems(elem, new, self.data_ptr(vec)?, i, n)?;
        let slice = self.vec_from_parts(new, n)?;
        Ok(self
            .copy_value(&Type::Vector(Box::new(elem.clone())), slice.into())?
            .into_struct_value())
    }

    /// `vector::replace_range`: replace [i, j) with `other`, returning the removed elements.
    /// Rebuilds the buffer (prefix ++ other ++ suffix); simple and O(len).
    pub(super) fn vec_replace_range(
        &self,
        elem: &Type,
        vec: PointerValue<'ctx>,
        i: IntValue<'ctx>,
        j: IntValue<'ctx>,
        other: StructValue<'ctx>,
    ) -> Result<StructValue<'ctx>> {
        let b = &self.builder;
        let len = self.vec_len(vec)?;
        self.range_check(i, j, len)?;
        let data = self.data_ptr(vec)?;
        let (odata, olen) = self.vec_parts(other)?;
        let esize = self.size_of(elem)?;

        let removed_n = b.build_int_sub(j, i, "")?;
        let removed = self.alloc(b.build_int_mul(removed_n, esize, "")?)?;
        self.copy_elems(elem, removed, data, i, removed_n)?;

        let b = &self.builder;
        let suffix = b.build_int_sub(len, j, "")?;
        let new_len = b.build_int_add(b.build_int_add(i, olen, "")?, suffix, "")?;
        let new = self.alloc(b.build_int_mul(new_len, esize, "")?)?;
        let zero = self.ctx.i64_type().const_zero();
        self.copy_elems(elem, new, data, zero, i)?;
        self.copy_elems(
            elem,
            self.elem_ptr_unchecked(elem, new, i)?,
            odata,
            zero,
            olen,
        )?;
        let at = self.builder.build_int_add(i, olen, "")?;
        self.copy_elems(
            elem,
            self.elem_ptr_unchecked(elem, new, at)?,
            data,
            j,
            suffix,
        )?;

        self.store_field(vec, DATA, new)?;
        self.store_field(vec, LEN, new_len)?;
        self.store_field(vec, CAP, new_len)?;
        self.vec_from_parts(removed, removed_n)
    }
}
