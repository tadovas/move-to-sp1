//! `__move_entry`: the glue between the runtime's `main` and the Move entry function, and
//! the program ABI recorded in the ELF (`.move2sp1.abi`) for the runner.
//!
//! Wire format (for now scalars only): every argument is one SP1 stdin buffer with the
//! value's little-endian bytes (bool = 1 byte, address = its 32 raw bytes); results are
//! appended to the public values in the same encoding.

use anyhow::{Result, bail};
use inkwell::{
    IntPredicate,
    module::Linkage,
    values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue},
};

use super::{FnId, FnInst, Translator, types::Type};

/// Signature of the entry function, as recorded for the runner.
pub struct EntryAbi {
    pub name: String,
    pub params: Vec<String>,
    pub returns: Vec<String>,
}

impl EntryAbi {
    pub fn to_text(&self) -> String {
        format!(
            "entry {}\nparams {}\nreturns {}\n",
            self.name,
            self.params.join(" "),
            self.returns.join(" ")
        )
    }
}

fn wire_bytes(ty: &Type) -> Result<u64> {
    match ty.int_bits() {
        Some(bits) => Ok(bits.div_ceil(8) as u64),
        None => bail!("entry functions can only take/return scalars for now (found {ty:?})"),
    }
}

impl<'ctx> Translator<'ctx, '_> {
    pub(super) fn entry_glue(
        &mut self,
        entry: FnId,
        target: FunctionValue<'ctx>,
    ) -> Result<EntryAbi> {
        let (params, returns) = self.signature(&FnInst {
            id: entry,
            type_args: vec![],
        })?;
        let i64t = self.ctx.i64_type();
        let glue = self.module.add_function(
            "__move_entry",
            self.ctx.void_type().fn_type(&[], false),
            Some(Linkage::External),
        );
        self.builder
            .position_at_end(self.ctx.append_basic_block(glue, "entry"));

        let mut args: Vec<BasicMetadataValueEnum> = vec![];
        for (i, ty) in params.iter().enumerate() {
            let n = wire_bytes(ty)?;
            let wire = self.int_type(n as u32 * 8);
            let slot = self.builder.build_alloca(wire, &format!("arg{i}"))?;
            self.builder.build_call(
                self.rt_read_arg(),
                &[slot.into(), i64t.const_int(n, false).into()],
                "",
            )?;
            let v = self.builder.build_load(wire, slot, "")?.into_int_value();
            let v = if *ty == Type::Bool {
                self.builder
                    .build_int_compare(IntPredicate::NE, v, wire.const_zero(), "")?
            } else {
                v
            };
            args.push(v.into());
        }

        let call = self.builder.build_call(target, &args, "ret")?;
        let results: Vec<BasicValueEnum> = match returns.len() {
            0 => vec![],
            1 => vec![call.try_as_basic_value().basic().unwrap()],
            n => {
                let agg = call
                    .try_as_basic_value()
                    .basic()
                    .unwrap()
                    .into_struct_value();
                (0..n)
                    .map(|i| Ok(self.builder.build_extract_value(agg, i as u32, "")?))
                    .collect::<Result<_>>()?
            }
        };
        for (v, ty) in results.into_iter().zip(&returns) {
            let n = wire_bytes(ty)?;
            let wire = self.int_type(n as u32 * 8);
            let v = self
                .builder
                .build_int_z_extend_or_bit_cast(v.into_int_value(), wire, "")?;
            let slot = self.builder.build_alloca(wire, "out")?;
            self.builder.build_store(slot, v)?;
            self.builder.build_call(
                self.rt_commit(),
                &[slot.into(), i64t.const_int(n, false).into()],
                "",
            )?;
        }
        self.builder.build_return(None)?;

        let names = |tys: &[Type]| tys.iter().map(|t| t.display(self.prog)).collect();
        Ok(EntryAbi {
            name: self.prog.function_name(entry),
            params: names(&params),
            returns: names(&returns),
        })
    }
}
