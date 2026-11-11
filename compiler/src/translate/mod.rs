//! Move bytecode -> LLVM IR.
//!
//! The translator works on a whole program (user modules + their dependencies) and emits a
//! single LLVM module, so LLVM can inline and optimize across Move module boundaries.
//!
//! * Functions are translated lazily from the entry point (worklist), only what is
//!   reachable is emitted. Generic functions are monomorphized: one LLVM function per
//!   `FnInst` (definition + concrete type arguments).
//! * Value representation (see [`Translator::llvm_type`]): integers are LLVM integers,
//!   structs are LLVM structs held by value, references are pointers, vectors are a
//!   `{ptr, len, cap}` header (see `vector.rs`), enums are a tagged union (`enums.rs`).
//! * Move semantics (overflow checks, bounds checks, deep copies, structural equality)
//!   are spelled out in `ops.rs`, `vector.rs` and `value.rs`.
//!
//! Module map:
//! * `program`  — loaded modules, cross-module resolution
//! * `types`    — monomorphic Move types
//! * `function` — per-function translation and bytecode dispatch
//! * `ops`      — arithmetic / comparison / cast semantics
//! * `value`    — copy, equality, constants
//! * `vector`   — vector layout and operations
//! * `enums`    — enum layout, variants and `VariantSwitch`
//! * `natives`  — native functions (`std::vector`, `bcs`, `hash`, `string`, `debug`)
//! * `descriptor` — type descriptors: value layouts the runtime walks (BCS, printing)
//! * `runtime`  — the `move_rt_*` runtime interface
//! * `entry`    — `__move_entry` glue and the program ABI

mod descriptor;
mod entry;
mod enums;
mod function;
mod natives;
mod ops;
mod program;
mod runtime;
mod types;
mod value;
mod vector;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use anyhow::{Result, anyhow, ensure};
use inkwell::{
    IntPredicate,
    builder::Builder,
    context::Context,
    module::{Linkage, Module},
    targets::TargetData,
    types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FunctionType, IntType, StructType},
    values::{FunctionValue, IntValue},
};

pub use entry::EntryAbi;
pub use program::{FnId, Program};
pub use types::Type;

/// A function instance: a definition plus concrete type arguments.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FnInst {
    pub id: FnId,
    pub type_args: Vec<Type>,
}

pub struct Translator<'ctx, 'p> {
    ctx: &'ctx Context,
    module: Module<'ctx>,
    builder: Builder<'ctx>,
    prog: &'p Program<'p>,
    /// Declared Move function instances.
    functions: HashMap<FnInst, FunctionValue<'ctx>>,
    /// Declared but not yet translated.
    queue: VecDeque<FnInst>,
    /// LLVM struct types for instantiated Move structs (memoized by `llvm_type`).
    struct_types: RefCell<HashMap<Type, StructType<'ctx>>>,
    /// Target layout (sizes/alignments for enum payloads).
    target_data: TargetData,
}

impl<'ctx, 'p> Translator<'ctx, 'p> {
    pub fn new(ctx: &'ctx Context, prog: &'p Program<'p>, target_data: TargetData) -> Self {
        Self {
            ctx,
            module: ctx.create_module("move"),
            builder: ctx.create_builder(),
            prog,
            functions: HashMap::new(),
            queue: VecDeque::new(),
            struct_types: RefCell::default(),
            target_data,
        }
    }

    /// Translate everything reachable from `entry` and generate the `__move_entry` glue.
    pub fn compile(&mut self, entry: FnId) -> Result<EntryAbi> {
        let inst = FnInst {
            id: entry,
            type_args: vec![],
        };
        let target = self.function(&inst)?;
        while let Some(inst) = self.queue.pop_front() {
            self.translate_function(&inst)
                .map_err(|e| anyhow!("in {}: {e:#}", self.instance_name(&inst)))?;
        }
        self.entry_glue(entry, target)
    }

    pub fn module(&self) -> &Module<'ctx> {
        &self.module
    }

    // ---- types

    fn int_type(&self, bits: u32) -> IntType<'ctx> {
        self.ctx
            .custom_width_int_type(std::num::NonZeroU32::new(bits).expect("zero-width int"))
            .expect("valid int width")
    }

    /// The LLVM representation of a Move type.
    fn llvm_type(&self, ty: &Type) -> Result<BasicTypeEnum<'ctx>> {
        Ok(match ty {
            Type::Vector(_) => self.vector_type().into(),
            Type::Ref(_) => self.ptr_type().into(),
            Type::Struct(..) | Type::Enum(..) if self.struct_types.borrow().contains_key(ty) => {
                self.struct_types.borrow()[ty].into()
            }
            Type::Enum(..) => self.enum_layout(ty)?.into(),
            Type::Struct(id, args) => {
                let st = self
                    .ctx
                    .opaque_struct_type(&format!("move.{}", ty.display(self.prog)));
                self.struct_types.borrow_mut().insert(ty.clone(), st);
                let fields = types::struct_fields(self.prog, *id, args)?
                    .iter()
                    .map(|f| self.llvm_type(f))
                    .collect::<Result<Vec<_>>>()?;
                st.set_body(&fields, false);
                st.into()
            }
            scalar => self.int_type(scalar.int_bits().unwrap()).into(),
        })
    }

    /// Return values: none -> void, one -> itself, several -> an anonymous struct.
    fn fn_type(&self, params: &[Type], returns: &[Type]) -> Result<FunctionType<'ctx>> {
        let params: Vec<BasicMetadataTypeEnum> = params
            .iter()
            .map(|t| self.llvm_type(t).map(Into::into))
            .collect::<Result<_>>()?;
        let rets: Vec<BasicTypeEnum> = returns
            .iter()
            .map(|t| self.llvm_type(t))
            .collect::<Result<_>>()?;
        Ok(match rets.as_slice() {
            [] => self.ctx.void_type().fn_type(&params, false),
            [r] => r.fn_type(&params, false),
            _ => self.ctx.struct_type(&rets, false).fn_type(&params, false),
        })
    }

    // ---- functions

    /// Parameter and return types of a function instance.
    fn signature(&self, inst: &FnInst) -> Result<(Vec<Type>, Vec<Type>)> {
        let cm = self.prog.module(inst.id.module);
        let h = cm.function_handle_at(cm.function_def_at(inst.id.def).function);
        ensure!(
            h.type_parameters.len() == inst.type_args.len(),
            "wrong number of type arguments"
        );
        let m = inst.id.module;
        Ok((
            Type::from_tokens(
                self.prog,
                m,
                &cm.signature_at(h.parameters).0,
                &inst.type_args,
            )?,
            Type::from_tokens(self.prog, m, &cm.signature_at(h.return_).0, &inst.type_args)?,
        ))
    }

    fn instance_name(&self, inst: &FnInst) -> String {
        format!(
            "{}{}",
            self.prog.function_name(inst.id),
            types::display_args(self.prog, &inst.type_args)
        )
    }

    /// Declare a Move function instance (translated later via the worklist).
    fn function(&mut self, inst: &FnInst) -> Result<FunctionValue<'ctx>> {
        if let Some(f) = self.functions.get(inst) {
            return Ok(*f);
        }
        let (params, returns) = self.signature(inst)?;
        let fty = self.fn_type(&params, &returns)?;
        let name = format!("move.{}", self.instance_name(inst));
        let f = self
            .module
            .add_function(&name, fty, Some(Linkage::Internal));
        self.functions.insert(inst.clone(), f);
        self.queue.push_back(inst.clone());
        Ok(f)
    }

    fn current_function(&self) -> FunctionValue<'ctx> {
        self.builder
            .get_insert_block()
            .and_then(|b| b.get_parent())
            .expect("builder is positioned inside a function")
    }

    /// Get or generate an internal helper function (deep copy, equality, ...). `body` is
    /// called with the builder positioned at the helper's entry block; the caller's
    /// insertion point is restored afterwards.
    fn helper(
        &mut self,
        name: &str,
        ty: FunctionType<'ctx>,
        body: impl FnOnce(&mut Self, FunctionValue<'ctx>) -> Result<()>,
    ) -> Result<FunctionValue<'ctx>> {
        if let Some(f) = self.module.get_function(name) {
            return Ok(f);
        }
        let f = self.module.add_function(name, ty, Some(Linkage::Internal));
        let saved = self.builder.get_insert_block();
        self.builder
            .position_at_end(self.ctx.append_basic_block(f, "entry"));
        body(self, f)?;
        if let Some(bb) = saved {
            self.builder.position_at_end(bb);
        }
        Ok(f)
    }

    /// Emit `for i in 0..n { body(i) }`; the builder continues after the loop.
    fn counted_loop(
        &mut self,
        n: IntValue<'ctx>,
        body: impl FnOnce(&mut Self, IntValue<'ctx>) -> Result<()>,
    ) -> Result<()> {
        let f = self.current_function();
        let i64t = self.ctx.i64_type();
        let pre = self.builder.get_insert_block().unwrap();
        let header = self.ctx.append_basic_block(f, "loop");
        let body_bb = self.ctx.append_basic_block(f, "loop.body");
        let exit = self.ctx.append_basic_block(f, "loop.exit");
        self.builder.build_unconditional_branch(header)?;

        self.builder.position_at_end(header);
        let i = self.builder.build_phi(i64t, "i")?;
        i.add_incoming(&[(&i64t.const_zero(), pre)]);
        let iv = i.as_basic_value().into_int_value();
        let done = self
            .builder
            .build_int_compare(IntPredicate::UGE, iv, n, "")?;
        self.builder.build_conditional_branch(done, exit, body_bb)?;

        self.builder.position_at_end(body_bb);
        body(self, iv)?;
        let next = self
            .builder
            .build_int_add(iv, i64t.const_int(1, false), "")?;
        i.add_incoming(&[(&next, self.builder.get_insert_block().unwrap())]);
        self.builder.build_unconditional_branch(header)?;

        self.builder.position_at_end(exit);
        Ok(())
    }

    /// Size in bytes of the LLVM representation of `ty` (as an i64 constant expression).
    fn size_of(&self, ty: &Type) -> Result<IntValue<'ctx>> {
        Ok(self.llvm_type(ty)?.size_of().expect("sized type"))
    }
}
