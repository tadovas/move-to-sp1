//! Enums.
//!
//! An enum value is `{ tag: i16, payload }` where `payload` is an integer array sized and
//! aligned for the largest variant. Each variant's fields form an anonymous LLVM struct that
//! is accessed by addressing the payload with that struct type. Operations work on memory
//! (an `alloca` for values); SROA/mem2reg clean that up.
//!
//! Unpacking the wrong variant aborts with `VARIANT_TAG_MISMATCH`, like the Move VM;
//! `VariantSwitch` becomes an LLVM `switch` over the tag.

use std::collections::HashMap;

use anyhow::Result;
use inkwell::{
    IntPredicate,
    basic_block::BasicBlock,
    types::StructType,
    values::{BasicValueEnum, IntValue, PointerValue},
};

use super::{
    Translator,
    runtime::VmError,
    types::{Type, variant_fields},
};

const TAG: u32 = 0;
const PAYLOAD: u32 = 1;

impl<'ctx> Translator<'ctx, '_> {
    /// LLVM layout of an instantiated enum (called by `llvm_type`, which memoizes it).
    pub(super) fn enum_layout(&self, ty: &Type) -> Result<StructType<'ctx>> {
        let Type::Enum(id, args) = ty else {
            unreachable!()
        };
        let st = self
            .ctx
            .opaque_struct_type(&format!("move.{}", ty.display(self.prog)));
        self.struct_types.borrow_mut().insert(ty.clone(), st);

        let (mut size, mut align) = (0u64, 1u32);
        for tag in 0..variant_fields(self.prog, *id, args)?.len() {
            let (vst, _) = self.variant_struct(ty, tag as u16)?;
            size = size.max(self.target_data.get_abi_size(&vst));
            align = align.max(self.target_data.get_abi_alignment(&vst));
        }
        let word = self.int_type(align * 8);
        let payload = word.array_type(size.div_ceil(align as u64) as u32);
        st.set_body(&[self.ctx.i16_type().into(), payload.into()], false);
        Ok(st)
    }

    /// The field struct of variant `tag`, and its field types.
    pub(super) fn variant_struct(
        &self,
        ty: &Type,
        tag: u16,
    ) -> Result<(StructType<'ctx>, Vec<Type>)> {
        let Type::Enum(id, args) = ty else {
            unreachable!()
        };
        let fields = variant_fields(self.prog, *id, args)?.swap_remove(tag as usize);
        let ltys = fields
            .iter()
            .map(|f| self.llvm_type(f))
            .collect::<Result<Vec<_>>>()?;
        Ok((self.ctx.struct_type(&ltys, false), fields))
    }

    pub(super) fn load_tag(&self, ty: &Type, p: PointerValue<'ctx>) -> Result<IntValue<'ctx>> {
        let st = self.llvm_type(ty)?.into_struct_type();
        let tp = self.builder.build_struct_gep(st, p, TAG, "tag.ptr")?;
        Ok(self
            .builder
            .build_load(self.ctx.i16_type(), tp, "tag")?
            .into_int_value())
    }

    fn payload_ptr(&self, ty: &Type, p: PointerValue<'ctx>) -> Result<PointerValue<'ctx>> {
        let st = self.llvm_type(ty)?.into_struct_type();
        Ok(self.builder.build_struct_gep(st, p, PAYLOAD, "payload")?)
    }

    /// Pointers to the fields of variant `tag` of the enum at `p`, after checking the tag.
    pub(super) fn variant_field_ptrs(
        &self,
        ty: &Type,
        p: PointerValue<'ctx>,
        tag: u16,
    ) -> Result<Vec<(PointerValue<'ctx>, Type)>> {
        let actual = self.load_tag(ty, p)?;
        let expected = self.ctx.i16_type().const_int(tag as u64, false);
        let mismatch = self
            .builder
            .build_int_compare(IntPredicate::NE, actual, expected, "")?;
        self.fail_if(mismatch, VmError::VariantTagMismatch)?;
        self.field_ptrs(ty, p, tag)
    }

    pub(super) fn field_ptrs(
        &self,
        ty: &Type,
        p: PointerValue<'ctx>,
        tag: u16,
    ) -> Result<Vec<(PointerValue<'ctx>, Type)>> {
        let (vst, fields) = self.variant_struct(ty, tag)?;
        let payload = self.payload_ptr(ty, p)?;
        fields
            .into_iter()
            .enumerate()
            .map(|(i, f)| {
                Ok((
                    self.builder.build_struct_gep(vst, payload, i as u32, "")?,
                    f,
                ))
            })
            .collect()
    }

    /// `PackVariant`: build an enum value from the variant's field values.
    pub(super) fn pack_variant(
        &self,
        ty: &Type,
        tag: u16,
        fields: &[BasicValueEnum<'ctx>],
    ) -> Result<BasicValueEnum<'ctx>> {
        let lty = self.llvm_type(ty)?;
        let tmp = self.builder.build_alloca(lty, "enum")?;
        let tp = self
            .builder
            .build_struct_gep(lty.into_struct_type(), tmp, TAG, "")?;
        self.builder
            .build_store(tp, self.ctx.i16_type().const_int(tag as u64, false))?;
        for ((p, _), v) in self.field_ptrs(ty, tmp, tag)?.into_iter().zip(fields) {
            self.builder.build_store(p, *v)?;
        }
        Ok(self.builder.build_load(lty, tmp, "")?)
    }

    /// `UnpackVariant` (by value): the variant's field values.
    pub(super) fn unpack_variant(
        &self,
        ty: &Type,
        tag: u16,
        v: BasicValueEnum<'ctx>,
    ) -> Result<Vec<(BasicValueEnum<'ctx>, Type)>> {
        let tmp = self.builder.build_alloca(self.llvm_type(ty)?, "enum")?;
        self.builder.build_store(tmp, v)?;
        self.variant_field_ptrs(ty, tmp, tag)?
            .into_iter()
            .map(|(p, f)| Ok((self.builder.build_load(self.llvm_type(&f)?, p, "")?, f)))
            .collect()
    }

    /// `VariantSwitch`: jump to `targets[tag]`.
    pub(super) fn variant_switch(
        &self,
        ty: &Type,
        p: PointerValue<'ctx>,
        targets: &[BasicBlock<'ctx>],
    ) -> Result<()> {
        let tag = self.load_tag(ty, p)?;
        let unreachable = self
            .ctx
            .append_basic_block(self.current_function(), "bad_tag");
        let cases: Vec<_> = targets
            .iter()
            .enumerate()
            .map(|(t, bb)| (self.ctx.i16_type().const_int(t as u64, false), *bb))
            .collect();
        self.builder.build_switch(tag, unreachable, &cases)?;
        self.builder.position_at_end(unreachable);
        self.builder.build_unreachable()?;
        Ok(())
    }

    /// Emit `match tag(p) { t => body(t, field pointers) }`; the builder continues after the
    /// match. Used for type-directed copy and equality.
    pub(super) fn for_each_variant(
        &mut self,
        ty: &Type,
        p: PointerValue<'ctx>,
        mut body: impl FnMut(&mut Self, u16, Vec<(PointerValue<'ctx>, Type)>) -> Result<()>,
    ) -> Result<()> {
        let Type::Enum(id, args) = ty else {
            unreachable!()
        };
        let n = variant_fields(self.prog, *id, args)?.len();
        let f = self.current_function();
        let join = self.ctx.append_basic_block(f, "variant.join");
        let blocks: HashMap<u16, BasicBlock> = (0..n as u16)
            .map(|t| (t, self.ctx.append_basic_block(f, &format!("variant{t}"))))
            .collect();
        let targets: Vec<_> = (0..n as u16).map(|t| blocks[&t]).collect();
        self.variant_switch(ty, p, &targets)?;
        for t in 0..n as u16 {
            self.builder.position_at_end(blocks[&t]);
            let fields = self.field_ptrs(ty, p, t)?;
            body(self, t, fields)?;
            self.builder.build_unconditional_branch(join)?;
        }
        self.builder.position_at_end(join);
        Ok(())
    }
}
