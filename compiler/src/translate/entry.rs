//! `__move_entry`: the glue between the runtime's `main` and the Move code, and the program
//! ABI recorded in the ELF (`.move2sp1.abi`) for the runner.
//!
//! I/O: SP1 stdin holds the input state snapshot (see `runtime/sui/src/state.rs`), then one
//! buffer per argument: the argument's BCS encoding, or for an object reference (`&T` /
//! `&mut T`) the 32-byte ID of an object in the input state (lent in place, so mutations
//! persist). `&Clock` is the clock singleton and takes no buffer. The public values receive
//! the results as BCS, then the platform's effects and the output state.
//!
//! Two kinds of entry: a Move function (`entry_glue`), or a package *publish*
//! (`publish_glue`) which runs every module's `init` with its one-time witness.
//!
//! The ABI is JSON:
//!
//! ```json
//! { "entry": "pool::swap", "params": ["&pool::Pool", "u64"], "returns": [...],
//!   "effects": [...], "fast": [], "types": { "pool::Pool": { "struct": [["reserve_a", "u64"]] } } }
//! ```

use std::collections::BTreeMap;

use anyhow::Result;
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

/// Canonical name of `sui::clock::Clock`.
const CLOCK: &str =
    "0000000000000000000000000000000000000000000000000000000000000002::clock::Clock";

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

    /// Add a field to the ABI JSON.
    pub(super) fn set(&mut self, key: &str, value: Vec<Value>) {
        self.json[key] = Value::Array(value);
    }
}

impl<'ctx> Translator<'ctx, '_> {
    /// Glue for a Move entry function: read arguments, call it, commit its results.
    pub(super) fn entry_glue(
        &mut self,
        entry: FnId,
        target: FunctionValue<'ctx>,
    ) -> Result<EntryAbi> {
        let (params, returns) = self.signature(&FnInst {
            id: entry,
            type_args: vec![],
        })?;
        self.begin_glue()?;
        let mut args: Vec<BasicMetadataValueEnum> = vec![];
        for ty in &params {
            args.push(self.read_param(ty)?);
        }
        // Sui: owned input objects must belong to the sender
        self.call_runtime("move_rt_sui_check_inputs", None, &[])?;
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
        self.finish_glue()?;
        self.abi(self.prog.function_name(entry), &params, &returns)
    }

    /// Glue for publishing: call each `init` (in order) with the transaction context from the
    /// single argument and, where expected, the module's one-time witness.
    pub(super) fn publish_glue(
        &mut self,
        inits: &[(FnId, FunctionValue<'ctx>)],
        tx_context: &Type,
    ) -> Result<EntryAbi> {
        self.begin_glue()?;
        let ctx_arg = self.read_param(&Type::Ref(Box::new(tx_context.clone())))?;
        for (id, f) in inits {
            let (params, _) = self.signature(&FnInst {
                id: *id,
                type_args: vec![],
            })?;
            let mut args = vec![];
            if let [otw, _] = params.as_slice() {
                // the one-time witness: the module's `MODULE { dummy_field: false }`
                args.push(self.llvm_type(otw)?.const_zero().into());
            }
            args.push(ctx_arg);
            self.builder.build_call(*f, &args, "")?;
        }
        self.finish_glue()?;
        self.abi("publish".into(), std::slice::from_ref(tx_context), &[])
    }

    pub(super) fn begin_glue(&mut self) -> Result<()> {
        let glue = self.module.add_function(
            "__move_entry",
            self.ctx.void_type().fn_type(&[], false),
            Some(Linkage::External),
        );
        self.builder
            .position_at_end(self.ctx.append_basic_block(glue, "entry"));
        // platform hook (Sui: load the input state)
        self.call_runtime("move_rt_start", None, &[])?;
        Ok(())
    }

    pub(super) fn finish_glue(&mut self) -> Result<()> {
        // platform hook (Sui: commit the effects and the output state)
        self.call_runtime("move_rt_finish", None, &[])?;
        self.builder.build_return(None)?;
        if self.module.get_global("__move_dispatch").is_none() {
            self.emit_empty_dispatch_table();
        }
        self.emit_type_registry()
    }

    /// One entry argument (see the module docs).
    pub(super) fn read_param(&mut self, ty: &Type) -> Result<BasicMetadataValueEnum<'ctx>> {
        let read = |t: &mut Self, ty: &Type| -> Result<_> {
            let slot = t.builder.build_alloca(t.llvm_type(ty)?, "arg")?;
            t.call_runtime(
                "move_rt_read_value",
                None,
                &[t.type_desc(ty)?.into(), slot.into()],
            )?;
            if ty.canonical(t.prog) == TX_CONTEXT {
                // Sui: the transaction context natives read comes from this argument
                t.call_runtime(
                    "move_rt_sui_tx_init",
                    None,
                    &[t.type_desc(ty)?.into(), slot.into()],
                )?;
            }
            Ok(slot)
        };
        Ok(match ty {
            Type::Ref(inner) if inner.canonical(self.prog) == TX_CONTEXT => {
                read(self, inner)?.into()
            }
            Type::Ref(inner) if inner.canonical(self.prog) == CLOCK => {
                let p = self.call_runtime(
                    "move_rt_sui_clock_borrow",
                    Some(self.ptr_type().into()),
                    &[self.type_desc(inner)?.into()],
                )?;
                p.unwrap().into()
            }
            Type::Ref(inner) => {
                // an object of the input state, by ID
                let id = read(self, &Type::Address)?;
                self.note_state_type(inner);
                let args = [id.into(), self.type_desc(inner)?.into()];
                self.call_runtime(
                    "move_rt_sui_borrow_object",
                    Some(self.ptr_type().into()),
                    &args,
                )?
                .unwrap()
                .into()
            }
            _ => {
                let slot = read(self, ty)?;
                self.builder
                    .build_load(self.llvm_type(ty)?, slot, "")?
                    .into()
            }
        })
    }

    /// An empty `__move_dispatch` (non-image programs; the runtime always links the PTB
    /// interpreter that reads it).
    fn emit_empty_dispatch_table(&self) {
        let i64t = self.ctx.i64_type();
        let g = self
            .module
            .add_global(i64t.array_type(0), None, "__move_dispatch");
        g.set_initializer(&i64t.const_array(&[]));
        g.set_constant(true);
        let len = self.module.add_global(i64t, None, "__move_dispatch_len");
        len.set_initializer(&i64t.const_zero());
        len.set_constant(true);
    }

    /// `__move_type_registry`: canonical name -> descriptor for every type that can be stored
    /// in the state, so the runtime can rebuild objects from a snapshot.
    fn emit_type_registry(&mut self) -> Result<()> {
        let i64t = self.ctx.i64_type();
        let entry_ty = self.ctx.struct_type(
            &[self.ptr_type().into(), i64t.into(), self.ptr_type().into()],
            false,
        );
        let mut entries = vec![];
        for ty in self.state_types.clone() {
            let (name, len) = self.const_bytes(ty.canonical(self.prog).as_bytes());
            let desc = self.type_desc(&ty)?;
            entries.push(entry_ty.const_named_struct(&[
                name.into(),
                i64t.const_int(len, false).into(),
                desc.into(),
            ]));
        }
        let arr = entry_ty.const_array(&entries);
        let g = self
            .module
            .add_global(arr.get_type(), None, "__move_type_registry");
        g.set_initializer(&arr);
        g.set_constant(true);
        let len = self
            .module
            .add_global(i64t, None, "__move_type_registry_len");
        len.set_initializer(&i64t.const_int(entries.len() as u64, false));
        len.set_constant(true);
        Ok(())
    }

    pub(super) fn abi(&self, name: String, params: &[Type], returns: &[Type]) -> Result<EntryAbi> {
        let mut types = BTreeMap::new();
        for ty in params
            .iter()
            .chain(returns)
            .chain(&self.effect_types)
            .chain(&self.state_types)
        {
            self.schema(ty, &mut types)?;
        }
        let names =
            |tys: &[Type]| -> Vec<String> { tys.iter().map(|t| t.display(self.prog)).collect() };
        let (params, returns) = (names(params), names(returns));
        let state: BTreeMap<String, String> = self
            .state_types
            .iter()
            .map(|t| (t.canonical(self.prog), t.display(self.prog)))
            .collect();
        let json = json!({
            "entry": name, "params": params, "returns": returns, "effects": names(&self.effect_types),
            "state": state, "types": types, "fast": self.options.fast_points(),
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
            Type::Vector(e) | Type::Ref(e) => self.schema(e, out)?,
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
