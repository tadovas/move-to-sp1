//! Program images: a set of callable function instances plus a dispatch table, so the
//! *transaction* (a PTB: which functions to call, with which arguments) is data read at run
//! time by the runtime's PTB interpreter (`runtime/sui/src/ptb.rs`), not code.
//!
//! Each entry gets a wrapper with a uniform ABI, `wrapper(args: *const *mut u8, rets: *const
//! *mut u8)`: `args[i]` points at a by-value argument's slot (loaded from) or *is* the
//! reference for a `&`/`&mut` parameter; return value `j` is stored at `rets[j]`.
//!
//! `__move_dispatch` (`__move_dispatch_len` entries), mirrored by the runtime:
//! `{ key, key_len, wrapper, n_params, params: *{ kind, desc }, n_returns, returns: *desc }`
//! with `kind` 0 = by value, 1 = `&`, 2 = `&mut` (`desc` is the referenced type's).
//! `key` is the canonical `<address>::<module>::<function>` with `<type args>` if any.

use anyhow::Result;
use inkwell::{
    module::Linkage,
    values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue},
};
use move_binary_format::file_format::SignatureToken;

use super::{EntryAbi, FnInst, Translator, types::Type};

/// Parameter kinds in the dispatch table.
const BY_VALUE: u64 = 0;
const IMM_REF: u64 = 1;
const MUT_REF: u64 = 2;

impl<'ctx> Translator<'ctx, '_> {
    /// Translate an image: every instance in `entries` callable through the dispatch table;
    /// objects of `types` can be loaded from the state too.
    pub fn compile_image(&mut self, entries: &[FnInst], types: &[Type]) -> Result<EntryAbi> {
        for ty in types {
            if matches!(ty, Type::Struct(..) | Type::Enum(..)) {
                self.note_state_type(ty);
            }
        }
        let mut fns = vec![];
        for inst in entries {
            fns.push((inst.clone(), self.function(inst)?));
        }
        self.drain()?;
        let mut table = vec![];
        for (inst, f) in &fns {
            table.push(self.dispatch_entry(inst, *f)?);
        }
        self.emit_dispatch_table(&table)?;

        // __move_entry: load state, read the TxContext, run the PTB (stdin), commit
        let tx_context = self.tx_context_type()?;
        self.begin_glue()?;
        let ctx = self.read_param(&Type::Ref(Box::new(tx_context.clone())))?;
        self.call_runtime("move_rt_sui_run_ptb", None, &[ctx])?;
        self.finish_glue()?;
        let mut abi = self.abi("image".into(), &[Type::Ref(Box::new(tx_context))], &[])?;
        abi.set("image", table.iter().map(|e| e.json.clone()).collect());
        Ok(abi)
    }

    /// The wrapper and table row of one instance.
    fn dispatch_entry(
        &mut self,
        inst: &FnInst,
        f: FunctionValue<'ctx>,
    ) -> Result<DispatchEntry<'ctx>> {
        let (params, returns) = self.signature(inst)?;
        let kinds = self.param_kinds(inst);
        let ptr = self.ptr_type();
        let wrapper_ty = self
            .ctx
            .void_type()
            .fn_type(&[ptr.into(), ptr.into()], false);
        let name = format!("move.dispatch.{}", self.instance_name(inst));
        let w = self
            .module
            .add_function(&name, wrapper_ty, Some(Linkage::Internal));
        let saved = self.builder.get_insert_block();
        self.builder
            .position_at_end(self.ctx.append_basic_block(w, "entry"));
        let (args_ptr, rets_ptr) = (
            w.get_nth_param(0).unwrap().into_pointer_value(),
            w.get_nth_param(1).unwrap().into_pointer_value(),
        );
        let i64t = self.ctx.i64_type();
        let mut args: Vec<BasicMetadataValueEnum> = vec![];
        for (i, ty) in params.iter().enumerate() {
            // SAFETY (IR level): the runtime passes one pointer per parameter
            let at = unsafe {
                self.builder.build_in_bounds_gep(
                    ptr,
                    args_ptr,
                    &[i64t.const_int(i as u64, false)],
                    "",
                )?
            };
            let p = self.builder.build_load(ptr, at, "")?.into_pointer_value();
            args.push(match ty {
                Type::Ref(_) => p.into(),
                _ => self.builder.build_load(self.llvm_type(ty)?, p, "")?.into(),
            });
        }
        let call = self.builder.build_call(f, &args, "")?;
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
        for (j, v) in results.into_iter().enumerate() {
            let at = unsafe {
                self.builder.build_in_bounds_gep(
                    ptr,
                    rets_ptr,
                    &[i64t.const_int(j as u64, false)],
                    "",
                )?
            };
            let out = self.builder.build_load(ptr, at, "")?.into_pointer_value();
            self.builder.build_store(out, v)?;
        }
        self.builder.build_return(None)?;
        if let Some(bb) = saved {
            self.builder.position_at_end(bb);
        }

        // objects passed in or out may live in the state
        for ty in params.iter().chain(&returns) {
            let inner = match ty {
                Type::Ref(t) => t.as_ref(),
                t => t,
            };
            if matches!(inner, Type::Struct(..) | Type::Enum(..)) {
                self.note_state_type(inner);
            }
        }
        let key = self.dispatch_key(inst);
        let json = serde_json::json!({
            "function": key,
            "params": params.iter().map(|t| t.display(self.prog)).collect::<Vec<_>>(),
            "returns": returns.iter().map(|t| t.display(self.prog)).collect::<Vec<_>>(),
        });
        Ok(DispatchEntry {
            key,
            wrapper: w,
            params,
            kinds,
            returns,
            json,
        })
    }

    /// `&` vs `&mut` (both are `Type::Ref`): from the declared signature.
    fn param_kinds(&self, inst: &FnInst) -> Vec<u64> {
        let cm = self.prog.module(inst.id.module);
        let h = cm.function_handle_at(cm.function_def_at(inst.id.def).function);
        cm.signature_at(h.parameters)
            .0
            .iter()
            .map(|t| match t {
                SignatureToken::Reference(_) => IMM_REF,
                SignatureToken::MutableReference(_) => MUT_REF,
                _ => BY_VALUE,
            })
            .collect()
    }

    /// `<64 hex address>::<module>::<function>` plus `<canonical type args>` if generic.
    pub(super) fn dispatch_key(&self, inst: &FnInst) -> String {
        let cm = self.prog.module(inst.id.module);
        let h = cm.function_handle_at(cm.function_def_at(inst.id.def).function);
        let mut key = format!(
            "{}::{}::{}",
            cm.address().to_canonical_string(false),
            cm.name(),
            cm.identifier_at(h.name)
        );
        if !inst.type_args.is_empty() {
            let args: Vec<String> = inst
                .type_args
                .iter()
                .map(|t| t.canonical(self.prog))
                .collect();
            key.push_str(&format!("<{}>", args.join(",")));
        }
        key
    }

    fn emit_dispatch_table(&mut self, table: &[DispatchEntry<'ctx>]) -> Result<()> {
        let (i64t, ptr) = (self.ctx.i64_type(), self.ptr_type());
        let param_ty = self.ctx.struct_type(&[i64t.into(), ptr.into()], false);
        let entry_ty = self.ctx.struct_type(
            &[
                ptr.into(),
                i64t.into(),
                ptr.into(),
                i64t.into(),
                ptr.into(),
                i64t.into(),
                ptr.into(),
            ],
            false,
        );
        let mut rows = vec![];
        for (n, e) in table.iter().enumerate() {
            let mut params = vec![];
            for (ty, kind) in e.params.iter().zip(&e.kinds) {
                let inner = match ty {
                    Type::Ref(t) => t.as_ref(),
                    t => t,
                };
                params.push(param_ty.const_named_struct(&[
                    i64t.const_int(*kind, false).into(),
                    self.type_desc(inner)?.into(),
                ]));
            }
            let mut rets = vec![];
            for ty in &e.returns {
                rets.push(self.type_desc(ty)?);
            }
            let params_g = self.const_global(
                &format!("move.dispatch.{n}.params"),
                param_ty.const_array(&params),
            );
            let rets_g = self.const_global(
                &format!("move.dispatch.{n}.returns"),
                ptr.const_array(&rets),
            );
            let (key, key_len) = self.const_bytes(e.key.as_bytes());
            rows.push(entry_ty.const_named_struct(&[
                key.into(),
                i64t.const_int(key_len, false).into(),
                e.wrapper.as_global_value().as_pointer_value().into(),
                i64t.const_int(e.params.len() as u64, false).into(),
                params_g.into(),
                i64t.const_int(e.returns.len() as u64, false).into(),
                rets_g.into(),
            ]));
        }
        let arr = entry_ty.const_array(&rows);
        let g = self
            .module
            .add_global(arr.get_type(), None, "__move_dispatch");
        g.set_initializer(&arr);
        g.set_constant(true);
        let len = self.module.add_global(i64t, None, "__move_dispatch_len");
        len.set_initializer(&i64t.const_int(rows.len() as u64, false));
        len.set_constant(true);
        Ok(())
    }

    fn const_global(
        &self,
        name: &str,
        value: inkwell::values::ArrayValue<'ctx>,
    ) -> inkwell::values::PointerValue<'ctx> {
        let g = self.module.add_global(value.get_type(), None, name);
        g.set_initializer(&value);
        g.set_constant(true);
        g.set_linkage(Linkage::Private);
        g.as_pointer_value()
    }
}

struct DispatchEntry<'ctx> {
    key: String,
    wrapper: FunctionValue<'ctx>,
    params: Vec<Type>,
    kinds: Vec<u64>,
    returns: Vec<Type>,
    json: serde_json::Value,
}
