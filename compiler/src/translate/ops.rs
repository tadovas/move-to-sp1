//! Integer semantics as defined by the Move VM: arithmetic aborts with
//! `ARITHMETIC_ERROR` on overflow, division by zero, shift amounts >= bit width and lossy
//! downcasts. All Move integers are unsigned.

use anyhow::Result;
use inkwell::{IntPredicate, intrinsics::Intrinsic, values::IntValue};
use move_binary_format::file_format::Bytecode;

use super::{Translator, runtime::VmError};

impl<'ctx> Translator<'ctx, '_> {
    pub(super) fn binary_op(
        &mut self,
        op: &Bytecode,
        l: IntValue<'ctx>,
        r: IntValue<'ctx>,
    ) -> Result<IntValue<'ctx>> {
        let b = &self.builder;
        Ok(match op {
            Bytecode::Add => self.checked("llvm.uadd.with.overflow", l, r)?,
            Bytecode::Sub => self.checked("llvm.usub.with.overflow", l, r)?,
            Bytecode::Mul => self.checked("llvm.umul.with.overflow", l, r)?,
            Bytecode::Div | Bytecode::Mod => {
                let zero =
                    b.build_int_compare(IntPredicate::EQ, r, r.get_type().const_zero(), "")?;
                self.fail_if(zero, VmError::Arithmetic)?;
                if matches!(op, Bytecode::Div) {
                    self.builder.build_int_unsigned_div(l, r, "")?
                } else {
                    self.builder.build_int_unsigned_rem(l, r, "")?
                }
            }
            Bytecode::Shl | Bytecode::Shr => {
                // rhs is a u8; shifting by >= bit width aborts (impossible for u256)
                let width = l.get_type().get_bit_width() as u64;
                if width < 256 {
                    let max = r.get_type().const_int(width, false);
                    let bad = b.build_int_compare(IntPredicate::UGE, r, max, "")?;
                    self.fail_if(bad, VmError::Arithmetic)?;
                }
                let b = &self.builder;
                let r = b.build_int_z_extend_or_bit_cast(r, l.get_type(), "")?;
                if matches!(op, Bytecode::Shl) {
                    b.build_left_shift(l, r, "")?
                } else {
                    b.build_right_shift(l, r, false, "")?
                }
            }
            Bytecode::BitOr | Bytecode::Or => b.build_or(l, r, "")?,
            Bytecode::BitAnd | Bytecode::And => b.build_and(l, r, "")?,
            Bytecode::Xor => b.build_xor(l, r, "")?,
            other => unreachable!("not a binary op: {other:?}"),
        })
    }

    /// `l op r` via an `llvm.*.with.overflow` intrinsic, aborting on overflow.
    fn checked(
        &mut self,
        intrinsic: &str,
        l: IntValue<'ctx>,
        r: IntValue<'ctx>,
    ) -> Result<IntValue<'ctx>> {
        let decl = Intrinsic::find(intrinsic)
            .and_then(|i| i.get_declaration(&self.module, &[l.get_type().into()]))
            .expect("overflow intrinsic");
        let res = self.builder.build_call(decl, &[l.into(), r.into()], "")?;
        let res = res
            .try_as_basic_value()
            .basic()
            .unwrap()
            .into_struct_value();
        let value = self
            .builder
            .build_extract_value(res, 0, "")?
            .into_int_value();
        let overflow = self
            .builder
            .build_extract_value(res, 1, "")?
            .into_int_value();
        self.fail_if(overflow, VmError::Arithmetic)?;
        Ok(value)
    }

    pub(super) fn compare(
        &self,
        op: &Bytecode,
        l: IntValue<'ctx>,
        r: IntValue<'ctx>,
    ) -> Result<IntValue<'ctx>> {
        let p = match op {
            Bytecode::Lt => IntPredicate::ULT,
            Bytecode::Gt => IntPredicate::UGT,
            Bytecode::Le => IntPredicate::ULE,
            Bytecode::Ge => IntPredicate::UGE,
            other => unreachable!("not a comparison: {other:?}"),
        };
        Ok(self.builder.build_int_compare(p, l, r, "")?)
    }

    /// `CastU*`: widening is free, narrowing aborts if the value does not fit.
    pub(super) fn cast(&mut self, v: IntValue<'ctx>, to_bits: u32) -> Result<IntValue<'ctx>> {
        let to = self.int_type(to_bits);
        let from_bits = v.get_type().get_bit_width();
        if to_bits >= from_bits {
            return Ok(self.builder.build_int_z_extend_or_bit_cast(v, to, "")?);
        }
        let max = v
            .get_type()
            .const_int_arbitrary_precision(&low_bits_mask(to_bits, from_bits));
        let lossy = self
            .builder
            .build_int_compare(IntPredicate::UGT, v, max, "")?;
        self.fail_if(lossy, VmError::Arithmetic)?;
        Ok(self.builder.build_int_truncate(v, to, "")?)
    }

    /// An integer constant of `bits` width from little-endian bytes.
    pub(super) fn const_int_le(&self, bits: u32, le: &[u8]) -> IntValue<'ctx> {
        let mut bytes = le.to_vec();
        bytes.resize(bytes.len().div_ceil(8) * 8, 0);
        let words: Vec<u64> = bytes
            .chunks(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        self.int_type(bits).const_int_arbitrary_precision(&words)
    }
}

/// 64-bit words (little-endian) of `2^bits - 1`, padded to `width` bits.
fn low_bits_mask(bits: u32, width: u32) -> Vec<u64> {
    (0..width.div_ceil(64))
        .map(|w| match bits.saturating_sub(w * 64) {
            0 => 0,
            n if n >= 64 => u64::MAX,
            n => (1u64 << n) - 1,
        })
        .collect()
}
