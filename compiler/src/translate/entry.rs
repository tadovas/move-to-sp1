//! `__move_entry`: the glue between the runtime's `main` and the Move entry function, and
//! the program ABI recorded in the ELF (`.move2sp1.abi`) for the runner.
//!
//! I/O format: every argument is one SP1 stdin buffer holding its BCS encoding; results are
//! appended to the public values as BCS, in order, followed by the effects the platform
//! runtime recorded (Sui: events and transfers, see `runtime/sui/src/effects.rs`). The ABI
//! is JSON (`effects` lists the types that may appear in effects):
//!
//! ```json
//! { "entry": "pool::swap", "params": ["pool::Pool", "u64"], "returns": [...],
//!   "types": { "pool::Pool": { "struct": [["reserve_a", "u64"], ...] },
//!              "m::Shape":   { "enum": [["Circle", [["r", "u64"]]], ["Empty", []]] } } }
//! ```

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use inkwell::{
    module::Linkage,
    values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue},
};
use serde_json::{Value, json};

use super::{
    FnId, FnInst, Translator,
    sui::TX_CONTEXT,
    types::{Type, struct_fields, variant_fields},
};

/// The program ABI, serialized into the ELF.
pub struct EntryAbi {
    pub name: String,
    pub params: Vec<String>,
    pub returns: Vec<String>,
    json: Value,
}

impl EntryAbi {
    pub fn to_json(&self) -> String {
        self.json.to_string()
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
        if let Some(r) = params
            .iter()
            .chain(&returns)
            .find(|t| matches!(t, Type::Ref(_)))
        {
            bail!(
                "entry functions cannot take or return references ({})",
                r.display(self.prog)
            );
        }
        let glue = self.module.add_function(
            "__move_entry",
            self.ctx.void_type().fn_type(&[], false),
            Some(Linkage::External),
        );
        self.builder
            .position_at_end(self.ctx.append_basic_block(glue, "entry"));

        let mut args: Vec<BasicMetadataValueEnum> = vec![];
        for ty in &params {
            let slot = self.builder.build_alloca(self.llvm_type(ty)?, "arg")?;
            self.call_runtime(
                "move_rt_read_value",
                None,
                &[self.type_desc(ty)?.into(), slot.into()],
            )?;
            if ty.canonical(self.prog) == TX_CONTEXT {
                // Sui: the transaction context natives read comes from this argument
                self.call_runtime(
                    "move_rt_sui_tx_init",
                    None,
                    &[self.type_desc(ty)?.into(), slot.into()],
                )?;
            }
            args.push(
                self.builder
                    .build_load(self.llvm_type(ty)?, slot, "")?
                    .into(),
            );
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
            let slot = self.spill(v)?;
            self.call_runtime(
                "move_rt_commit_value",
                None,
                &[self.type_desc(ty)?.into(), slot.into()],
            )?;
        }
        // platform hook (Sui: commit the effects log)
        self.call_runtime("move_rt_finish", None, &[])?;
        self.builder.build_return(None)?;

        let mut types = BTreeMap::new();
        let effect_types = self.effect_types.clone();
        for ty in params.iter().chain(&returns).chain(&effect_types) {
            self.schema(ty, &mut types)?;
        }
        let names =
            |tys: &[Type]| -> Vec<String> { tys.iter().map(|t| t.display(self.prog)).collect() };
        let (name, params, returns) = (
            self.prog.function_name(entry),
            names(&params),
            names(&returns),
        );
        let effects = names(&effect_types);
        let json = json!({
            "entry": name, "params": params, "returns": returns, "effects": effects, "types": types,
            "fast": self.options.fast_points(),
        });
        Ok(EntryAbi {
            name,
            params,
            returns,
            json,
        })
    }

    /// Record the field layout of every struct/enum reachable from `ty`.
    fn schema(&self, ty: &Type, out: &mut BTreeMap<String, Value>) -> Result<()> {
        let name = ty.display(self.prog);
        if out.contains_key(&name) {
            return Ok(());
        }
        let field_list = |names: Vec<String>, tys: &[Type]| -> Vec<Value> {
            names
                .into_iter()
                .zip(tys)
                .map(|(n, t)| json!([n, t.display(self.prog)]))
                .collect()
        };
        match ty {
            Type::Vector(e) => self.schema(e, out)?,
            Type::Struct(id, args) => {
                let tys = struct_fields(self.prog, *id, args)?;
                let cm = self.prog.module(id.module);
                let names = cm
                    .struct_def_at(id.def)
                    .fields()
                    .unwrap()
                    .iter()
                    .map(|f| cm.identifier_at(f.name).to_string())
                    .collect();
                out.insert(name, json!({ "struct": field_list(names, &tys) }));
                for t in &tys {
                    self.schema(t, out)?;
                }
            }
            Type::Enum(id, args) => {
                let cm = self.prog.module(id.module);
                let def = cm.enum_def_at(id.def);
                let all = variant_fields(self.prog, *id, args)?;
                let variants: Vec<Value> = def
                    .variants
                    .iter()
                    .zip(&all)
                    .map(|(v, tys)| {
                        let names = v
                            .fields
                            .iter()
                            .map(|f| cm.identifier_at(f.name).to_string())
                            .collect();
                        json!([
                            cm.identifier_at(v.variant_name).as_str(),
                            field_list(names, tys)
                        ])
                    })
                    .collect();
                out.insert(name, json!({ "enum": variants }));
                for t in all.iter().flatten() {
                    self.schema(t, out)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
