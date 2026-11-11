//! Move bytecode -> LLVM IR.
//!
//! Strategy
//! * One LLVM function per Move function (`move.<module>.<function>`), translated lazily from
//!   the entry point through a worklist, so only reachable code is emitted.
//! * Move locals (params included) become `alloca`s; LLVM's mem2reg turns them into SSA.
//! * The Move operand stack is simulated at compile time: the bytecode verifier guarantees the
//!   stack is empty at basic-block boundaries, so within a block it is just a Vec of SSA values.
//! * Arithmetic follows Move semantics: overflow / division by zero / oversized shift /
//!   lossy cast abort via `move_rt_arith_error`.
//! * `__move_entry` is generated as the glue the runtime's `main` calls: it reads each
//!   argument from SP1 stdin, calls the entry function and commits the results as public values.

use std::collections::{BTreeSet, HashMap, VecDeque};

use anyhow::{Result, anyhow, bail, ensure};
use inkwell::{
    AddressSpace, IntPredicate,
    basic_block::BasicBlock,
    builder::Builder,
    context::Context,
    intrinsics::Intrinsic,
    module::{Linkage, Module},
    types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, IntType},
    values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue},
};
use move_binary_format::{
    CompiledModule,
    file_format::{Bytecode, FunctionDefinitionIndex, FunctionHandleIndex, SignatureToken},
};
use move_core_types::{account_address::AccountAddress, identifier::IdentStr};

/// A Move function, identified by (module index in `Program`, function definition index).
type FnId = (usize, FunctionDefinitionIndex);

pub struct Program<'a> {
    pub modules: Vec<&'a CompiledModule>,
}

impl<'a> Program<'a> {
    fn find_module(&self, addr: &AccountAddress, name: &IdentStr) -> Option<usize> {
        self.modules
            .iter()
            .position(|m| m.address() == addr && m.name() == name)
    }

    fn find_function(&self, mi: usize, name: &IdentStr) -> Option<FunctionDefinitionIndex> {
        let m = self.modules[mi];
        m.function_defs()
            .iter()
            .position(|fd| m.identifier_at(m.function_handle_at(fd.function).name) == name)
            .map(|i| FunctionDefinitionIndex(i as u16))
    }

    /// Resolve a call site's handle (possibly into another module) to a definition.
    fn resolve_call(&self, mi: usize, fh: FunctionHandleIndex) -> Result<FnId> {
        let m = self.modules[mi];
        let h = m.function_handle_at(fh);
        let mh = m.module_handle_at(h.module);
        let addr = m.address_identifier_at(mh.address);
        let mname = m.identifier_at(mh.name);
        let fname = m.identifier_at(h.name);
        let tm = self
            .find_module(addr, mname)
            .ok_or_else(|| anyhow!("module {addr}::{mname} not available"))?;
        let fd = self
            .find_function(tm, fname)
            .ok_or_else(|| anyhow!("function {mname}::{fname} not found"))?;
        Ok((tm, fd))
    }

    pub fn fn_name(&self, (mi, fd): FnId) -> String {
        let m = self.modules[mi];
        let h = m.function_handle_at(m.function_def_at(fd).function);
        format!("{}::{}", m.name(), m.identifier_at(h.name))
    }

    /// Find `module::function` (or just `function` if unambiguous) among the given modules.
    pub fn lookup_entry(&self, spec: &str, candidates: &[usize]) -> Result<FnId> {
        let (mname, fname) = match spec.rsplit_once("::") {
            Some((m, f)) => (Some(m.rsplit("::").next().unwrap()), f),
            None => (None, spec),
        };
        let mut found = vec![];
        for &mi in candidates {
            let m = self.modules[mi];
            if mname.is_some_and(|n| m.name().as_str() != n) {
                continue;
            }
            if let Some(fd) = self.find_function(mi, IdentStr::new(fname)?) {
                found.push((mi, fd));
            }
        }
        match found.len() {
            1 => Ok(found[0]),
            0 => bail!("entry function `{spec}` not found"),
            _ => bail!("entry function `{spec}` is ambiguous; use module::function"),
        }
    }
}

/// Scalar Move types this translator handles so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Address,
}

impl Scalar {
    pub fn from_token(t: &SignatureToken) -> Result<Self> {
        Ok(match t {
            SignatureToken::Bool => Scalar::Bool,
            SignatureToken::U8 => Scalar::U8,
            SignatureToken::U16 => Scalar::U16,
            SignatureToken::U32 => Scalar::U32,
            SignatureToken::U64 => Scalar::U64,
            SignatureToken::U128 => Scalar::U128,
            SignatureToken::U256 => Scalar::U256,
            SignatureToken::Address => Scalar::Address,
            other => bail!("type {other:?} not supported yet"),
        })
    }
    pub fn bits(self) -> u32 {
        match self {
            Scalar::Bool => 1,
            Scalar::U8 => 8,
            Scalar::U16 => 16,
            Scalar::U32 => 32,
            Scalar::U64 => 64,
            Scalar::U128 => 128,
            Scalar::U256 | Scalar::Address => 256,
        }
    }
    /// Size of the value's wire encoding (stdin args / public values), little-endian.
    pub fn bytes(self) -> u64 {
        (self.bits() as u64).div_ceil(8)
    }
    pub fn name(self) -> &'static str {
        match self {
            Scalar::Bool => "bool",
            Scalar::U8 => "u8",
            Scalar::U16 => "u16",
            Scalar::U32 => "u32",
            Scalar::U64 => "u64",
            Scalar::U128 => "u128",
            Scalar::U256 => "u256",
            Scalar::Address => "address",
        }
    }
}

pub struct Codegen<'ctx, 'p> {
    ctx: &'ctx Context,
    pub module: Module<'ctx>,
    builder: Builder<'ctx>,
    prog: &'p Program<'p>,
    functions: HashMap<FnId, FunctionValue<'ctx>>,
    queue: VecDeque<FnId>,
}

/// Signature of the generated entry glue, recorded into the ELF for the runner.
pub struct EntryAbi {
    pub name: String,
    pub params: Vec<Scalar>,
    pub returns: Vec<Scalar>,
}

impl EntryAbi {
    pub fn to_text(&self) -> String {
        let j = |v: &[Scalar]| v.iter().map(|s| s.name()).collect::<Vec<_>>().join(" ");
        format!(
            "entry {}\nparams {}\nreturns {}\n",
            self.name,
            j(&self.params),
            j(&self.returns)
        )
    }
}

impl<'ctx, 'p> Codegen<'ctx, 'p> {
    pub fn new(ctx: &'ctx Context, prog: &'p Program<'p>) -> Self {
        Self {
            ctx,
            module: ctx.create_module("move"),
            builder: ctx.create_builder(),
            prog,
            functions: HashMap::new(),
            queue: VecDeque::new(),
        }
    }

    fn iw(&self, bits: u32) -> IntType<'ctx> {
        self.ctx
            .custom_width_int_type(std::num::NonZeroU32::new(bits).expect("zero-width int"))
            .expect("valid int width")
    }

    fn int_ty(&self, s: Scalar) -> IntType<'ctx> {
        self.iw(s.bits())
    }

    fn ty(&self, t: &SignatureToken) -> Result<BasicTypeEnum<'ctx>> {
        Ok(self.int_ty(Scalar::from_token(t)?).into())
    }

    fn rt_fn(
        &self,
        name: &str,
        ret: Option<BasicTypeEnum<'ctx>>,
        params: &[BasicMetadataTypeEnum<'ctx>],
        noreturn: bool,
    ) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function(name) {
            return f;
        }
        let fty = match ret {
            Some(r) => r.fn_type(params, false),
            None => self.ctx.void_type().fn_type(params, false),
        };
        let f = self.module.add_function(name, fty, Some(Linkage::External));
        if noreturn {
            let kind = inkwell::attributes::Attribute::get_named_enum_kind_id("noreturn");
            f.add_attribute(
                inkwell::attributes::AttributeLoc::Function,
                self.ctx.create_enum_attribute(kind, 0),
            );
        }
        f
    }

    fn arith_error(&self) -> FunctionValue<'ctx> {
        self.rt_fn("move_rt_arith_error", None, &[], true)
    }

    /// Declare (and enqueue for translation) a Move function.
    fn function(&mut self, id: FnId) -> Result<FunctionValue<'ctx>> {
        if let Some(f) = self.functions.get(&id) {
            return Ok(*f);
        }
        let m = self.prog.modules[id.0];
        let h = m.function_handle_at(m.function_def_at(id.1).function);
        ensure!(
            h.type_parameters.is_empty(),
            "generic functions not supported yet ({})",
            self.prog.fn_name(id)
        );
        let params: Vec<BasicMetadataTypeEnum> = m
            .signature_at(h.parameters)
            .0
            .iter()
            .map(|t| self.ty(t).map(Into::into))
            .collect::<Result<_>>()?;
        let rets: Vec<BasicTypeEnum> = m
            .signature_at(h.return_)
            .0
            .iter()
            .map(|t| self.ty(t))
            .collect::<Result<_>>()?;
        let fty = match rets.len() {
            0 => self.ctx.void_type().fn_type(&params, false),
            1 => rets[0].fn_type(&params, false),
            _ => self.ctx.struct_type(&rets, false).fn_type(&params, false),
        };
        let name = format!("move.{}", self.prog.fn_name(id).replace("::", "."));
        let f = self
            .module
            .add_function(&name, fty, Some(Linkage::Internal));
        self.functions.insert(id, f);
        self.queue.push_back(id);
        Ok(f)
    }

    /// Translate everything reachable from `entry` and generate `__move_entry`.
    pub fn compile(&mut self, entry: FnId) -> Result<EntryAbi> {
        let f = self.function(entry)?;
        while let Some(id) = self.queue.pop_front() {
            self.translate(id)
                .map_err(|e| anyhow!("in {}: {e}", self.prog.fn_name(id)))?;
        }
        self.entry_glue(entry, f)
    }

    fn entry_glue(&mut self, entry: FnId, target: FunctionValue<'ctx>) -> Result<EntryAbi> {
        let m = self.prog.modules[entry.0];
        let h = m.function_handle_at(m.function_def_at(entry.1).function);
        let params: Vec<Scalar> = m
            .signature_at(h.parameters)
            .0
            .iter()
            .map(Scalar::from_token)
            .collect::<Result<_>>()?;
        let returns: Vec<Scalar> = m
            .signature_at(h.return_)
            .0
            .iter()
            .map(Scalar::from_token)
            .collect::<Result<_>>()?;

        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let i64t = self.ctx.i64_type();
        let read_arg = self.rt_fn("move_rt_read_arg", None, &[ptr.into(), i64t.into()], false);
        let commit = self.rt_fn("move_rt_commit", None, &[ptr.into(), i64t.into()], false);

        let glue = self.module.add_function(
            "__move_entry",
            self.ctx.void_type().fn_type(&[], false),
            Some(Linkage::External),
        );
        let b = &self.builder;
        b.position_at_end(self.ctx.append_basic_block(glue, "entry"));

        // Wire format: each scalar is its little-endian bytes (bool = 1 byte).
        let mut args: Vec<BasicMetadataValueEnum> = vec![];
        for (i, s) in params.iter().enumerate() {
            let wire = self.iw(s.bytes() as u32 * 8);
            let slot = b.build_alloca(wire, &format!("arg{i}"))?;
            b.build_call(
                read_arg,
                &[slot.into(), i64t.const_int(s.bytes(), false).into()],
                "",
            )?;
            let v = b.build_load(wire, slot, "")?.into_int_value();
            let v = if s.bits() < wire.get_bit_width() {
                // bool: any non-zero byte is true
                b.build_int_compare(IntPredicate::NE, v, wire.const_zero(), "")?
            } else {
                v
            };
            args.push(v.into());
        }
        let call = b.build_call(target, &args, "ret")?;
        let rets: Vec<IntValue> = match returns.len() {
            0 => vec![],
            1 => vec![call.try_as_basic_value().basic().unwrap().into_int_value()],
            n => {
                let agg = call
                    .try_as_basic_value()
                    .basic()
                    .unwrap()
                    .into_struct_value();
                (0..n)
                    .map(|i| Ok(b.build_extract_value(agg, i as u32, "")?.into_int_value()))
                    .collect::<Result<_>>()?
            }
        };
        for (v, s) in rets.into_iter().zip(&returns) {
            let wire = self.iw(s.bytes() as u32 * 8);
            let v = b.build_int_z_extend_or_bit_cast(v, wire, "")?;
            let slot = b.build_alloca(wire, "out")?;
            b.build_store(slot, v)?;
            b.build_call(
                commit,
                &[slot.into(), i64t.const_int(s.bytes(), false).into()],
                "",
            )?;
        }
        b.build_return(None)?;

        Ok(EntryAbi {
            name: self.prog.fn_name(entry),
            params,
            returns,
        })
    }

    fn translate(&mut self, id: FnId) -> Result<()> {
        let prog = self.prog;
        let m = prog.modules[id.0];
        let def = m.function_def_at(id.1);
        let code = def
            .code
            .as_ref()
            .ok_or_else(|| anyhow!("native function {} not supported yet", prog.fn_name(id)))?;
        let h = m.function_handle_at(def.function);
        let f = self.functions[&id];

        let param_tys = &m.signature_at(h.parameters).0;
        let local_tys: Vec<SignatureToken> = param_tys
            .iter()
            .chain(m.signature_at(code.locals).0.iter())
            .cloned()
            .collect();
        let ret_tys = &m.signature_at(h.return_).0;

        // --- basic block leaders
        let n = code.code.len();
        let mut leaders: BTreeSet<usize> = BTreeSet::from([0]);
        for (pc, ins) in code.code.iter().enumerate() {
            match ins {
                Bytecode::BrTrue(t) | Bytecode::BrFalse(t) | Bytecode::Branch(t) => {
                    leaders.insert(*t as usize);
                    leaders.insert(pc + 1);
                }
                Bytecode::Ret | Bytecode::Abort => {
                    leaders.insert(pc + 1);
                }
                Bytecode::VariantSwitch(_) => bail!("enums not supported yet"),
                _ => {}
            }
        }
        leaders.retain(|&l| l < n);

        let entry_bb = self.ctx.append_basic_block(f, "entry");
        let blocks: HashMap<usize, BasicBlock> = leaders
            .iter()
            .map(|&pc| (pc, self.ctx.append_basic_block(f, &format!("pc{pc}"))))
            .collect();

        // --- locals
        let b = &self.builder;
        b.position_at_end(entry_bb);
        let mut locals: Vec<(PointerValue, BasicTypeEnum)> = vec![];
        for (i, t) in local_tys.iter().enumerate() {
            let ty = self.ty(t)?;
            let slot = b.build_alloca(ty, &format!("l{i}"))?;
            if i < param_tys.len() {
                b.build_store(slot, f.get_nth_param(i as u32).unwrap())?;
            }
            locals.push((slot, ty));
        }
        b.build_unconditional_branch(blocks[&0])?;

        // --- instructions
        let mut stack: Vec<BasicValueEnum<'ctx>> = vec![];
        let mut terminated = true;
        for (pc, ins) in code.code.iter().enumerate() {
            if let Some(bb) = blocks.get(&pc) {
                if !terminated {
                    self.builder.build_unconditional_branch(*bb)?;
                }
                ensure!(
                    stack.is_empty(),
                    "operand stack not empty at block boundary (pc {pc})"
                );
                self.builder.position_at_end(*bb);
                terminated = false;
            } else if terminated {
                continue; // unreachable code after a terminator
            }

            let b = &self.builder;
            let pop = |s: &mut Vec<BasicValueEnum<'ctx>>| {
                s.pop().ok_or_else(|| anyhow!("stack underflow at pc {pc}"))
            };
            match ins {
                Bytecode::Nop => {}
                Bytecode::Pop => {
                    pop(&mut stack)?;
                }
                Bytecode::LdTrue => stack.push(self.ctx.bool_type().const_int(1, false).into()),
                Bytecode::LdFalse => stack.push(self.ctx.bool_type().const_zero().into()),
                Bytecode::LdU8(v) => {
                    stack.push(self.ctx.i8_type().const_int(*v as u64, false).into())
                }
                Bytecode::LdU16(v) => {
                    stack.push(self.ctx.i16_type().const_int(*v as u64, false).into())
                }
                Bytecode::LdU32(v) => {
                    stack.push(self.ctx.i32_type().const_int(*v as u64, false).into())
                }
                Bytecode::LdU64(v) => stack.push(self.ctx.i64_type().const_int(*v, false).into()),
                Bytecode::LdU128(v) => {
                    let v = **v;
                    let words = [v as u64, (v >> 64) as u64];
                    stack.push(
                        self.ctx
                            .i128_type()
                            .const_int_arbitrary_precision(&words)
                            .into(),
                    )
                }
                Bytecode::LdU256(v) => {
                    let bytes = v.to_le_bytes();
                    let words: Vec<u64> = bytes
                        .chunks(8)
                        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
                        .collect();
                    stack.push(self.iw(256).const_int_arbitrary_precision(&words).into())
                }
                Bytecode::LdConst(idx) => {
                    let c = m.constant_at(*idx);
                    let s = Scalar::from_token(&c.type_)?;
                    let mut bytes = c.data.clone();
                    ensure!(
                        bytes.len() as u64 == s.bytes(),
                        "unexpected constant encoding"
                    );
                    bytes.resize(bytes.len().div_ceil(8) * 8, 0);
                    let words: Vec<u64> = bytes
                        .chunks(8)
                        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
                        .collect();
                    stack.push(self.int_ty(s).const_int_arbitrary_precision(&words).into());
                }
                Bytecode::CopyLoc(i) | Bytecode::MoveLoc(i) => {
                    let (slot, ty) = locals[*i as usize];
                    stack.push(b.build_load(ty, slot, &format!("l{i}"))?);
                }
                Bytecode::StLoc(i) => {
                    let v = pop(&mut stack)?;
                    b.build_store(locals[*i as usize].0, v)?;
                }
                Bytecode::Add | Bytecode::Sub | Bytecode::Mul => {
                    let r = pop(&mut stack)?.into_int_value();
                    let l = pop(&mut stack)?.into_int_value();
                    let name = match ins {
                        Bytecode::Add => "llvm.uadd.with.overflow",
                        Bytecode::Sub => "llvm.usub.with.overflow",
                        _ => "llvm.umul.with.overflow",
                    };
                    let intr = Intrinsic::find(name).unwrap();
                    let decl = intr
                        .get_declaration(&self.module, &[l.get_type().into()])
                        .unwrap();
                    let res = b
                        .build_call(decl, &[l.into(), r.into()], "")?
                        .try_as_basic_value()
                        .basic()
                        .unwrap()
                        .into_struct_value();
                    let v = b.build_extract_value(res, 0, "")?.into_int_value();
                    let ovf = b.build_extract_value(res, 1, "")?.into_int_value();
                    self.check(f, ovf)?;
                    stack.push(v.into());
                }
                Bytecode::Div | Bytecode::Mod => {
                    let r = pop(&mut stack)?.into_int_value();
                    let l = pop(&mut stack)?.into_int_value();
                    let zero =
                        b.build_int_compare(IntPredicate::EQ, r, r.get_type().const_zero(), "")?;
                    self.check(f, zero)?;
                    let b = &self.builder;
                    let v = if matches!(ins, Bytecode::Div) {
                        b.build_int_unsigned_div(l, r, "")?
                    } else {
                        b.build_int_unsigned_rem(l, r, "")?
                    };
                    stack.push(v.into());
                }
                Bytecode::Shl | Bytecode::Shr => {
                    let r = pop(&mut stack)?.into_int_value(); // u8
                    let l = pop(&mut stack)?.into_int_value();
                    let w = l.get_type().get_bit_width() as u64;
                    let too_big = b.build_int_compare(
                        IntPredicate::UGE,
                        r,
                        self.ctx.i8_type().const_int(w.min(255), false),
                        "",
                    )?;
                    // u256: any u8 shift is valid (max 255 < 256)
                    if w <= 255 {
                        self.check(f, too_big)?;
                    }
                    let b = &self.builder;
                    let r = b.build_int_z_extend_or_bit_cast(r, l.get_type(), "")?;
                    let v = if matches!(ins, Bytecode::Shl) {
                        b.build_left_shift(l, r, "")?
                    } else {
                        b.build_right_shift(l, r, false, "")?
                    };
                    stack.push(v.into());
                }
                Bytecode::BitOr
                | Bytecode::BitAnd
                | Bytecode::Xor
                | Bytecode::Or
                | Bytecode::And => {
                    let r = pop(&mut stack)?.into_int_value();
                    let l = pop(&mut stack)?.into_int_value();
                    let v = match ins {
                        Bytecode::BitOr | Bytecode::Or => b.build_or(l, r, "")?,
                        Bytecode::BitAnd | Bytecode::And => b.build_and(l, r, "")?,
                        _ => b.build_xor(l, r, "")?,
                    };
                    stack.push(v.into());
                }
                Bytecode::Not => {
                    let v = pop(&mut stack)?.into_int_value();
                    stack.push(b.build_not(v, "")?.into());
                }
                Bytecode::Eq
                | Bytecode::Neq
                | Bytecode::Lt
                | Bytecode::Gt
                | Bytecode::Le
                | Bytecode::Ge => {
                    let r = pop(&mut stack)?.into_int_value();
                    let l = pop(&mut stack)?.into_int_value();
                    let p = match ins {
                        Bytecode::Eq => IntPredicate::EQ,
                        Bytecode::Neq => IntPredicate::NE,
                        Bytecode::Lt => IntPredicate::ULT,
                        Bytecode::Gt => IntPredicate::UGT,
                        Bytecode::Le => IntPredicate::ULE,
                        _ => IntPredicate::UGE,
                    };
                    stack.push(b.build_int_compare(p, l, r, "")?.into());
                }
                Bytecode::CastU8
                | Bytecode::CastU16
                | Bytecode::CastU32
                | Bytecode::CastU64
                | Bytecode::CastU128
                | Bytecode::CastU256 => {
                    let v = pop(&mut stack)?.into_int_value();
                    let to = self.iw(match ins {
                        Bytecode::CastU8 => 8,
                        Bytecode::CastU16 => 16,
                        Bytecode::CastU32 => 32,
                        Bytecode::CastU64 => 64,
                        Bytecode::CastU128 => 128,
                        _ => 256,
                    });
                    let from_w = v.get_type().get_bit_width();
                    let to_w = to.get_bit_width();
                    let out = if to_w >= from_w {
                        b.build_int_z_extend_or_bit_cast(v, to, "")?
                    } else {
                        let max = v
                            .get_type()
                            .const_int_arbitrary_precision(&max_words(to_w, from_w));
                        let lossy = b.build_int_compare(IntPredicate::UGT, v, max, "")?;
                        self.check(f, lossy)?;
                        self.builder.build_int_truncate(v, to, "")?
                    };
                    stack.push(out.into());
                }
                Bytecode::BrTrue(t) | Bytecode::BrFalse(t) => {
                    let c = pop(&mut stack)?.into_int_value();
                    ensure!(
                        stack.is_empty(),
                        "operand stack not empty at branch (pc {pc})"
                    );
                    let (then, els) = (blocks[&(*t as usize)], blocks[&(pc + 1)]);
                    if matches!(ins, Bytecode::BrTrue(_)) {
                        b.build_conditional_branch(c, then, els)?;
                    } else {
                        b.build_conditional_branch(c, els, then)?;
                    }
                    terminated = true;
                }
                Bytecode::Branch(t) => {
                    ensure!(
                        stack.is_empty(),
                        "operand stack not empty at branch (pc {pc})"
                    );
                    b.build_unconditional_branch(blocks[&(*t as usize)])?;
                    terminated = true;
                }
                Bytecode::Ret => {
                    let k = ret_tys.len();
                    ensure!(stack.len() >= k, "stack underflow at ret");
                    let vals: Vec<BasicValueEnum> = stack.split_off(stack.len() - k);
                    match k {
                        0 => b.build_return(None)?,
                        1 => b.build_return(Some(&vals[0]))?,
                        _ => b.build_aggregate_return(&vals)?,
                    };
                    terminated = true;
                }
                Bytecode::Abort => {
                    let code = pop(&mut stack)?.into_int_value();
                    let abort =
                        self.rt_fn("move_rt_abort", None, &[self.ctx.i64_type().into()], true);
                    let b = &self.builder;
                    b.build_call(abort, &[code.into()], "")?;
                    b.build_unreachable()?;
                    stack.clear();
                    terminated = true;
                }
                Bytecode::Call(fh) => {
                    let callee_id = prog.resolve_call(id.0, *fh)?;
                    let callee = self.function(callee_id)?;
                    let argc = callee.count_params() as usize;
                    ensure!(stack.len() >= argc, "stack underflow at call");
                    let args: Vec<BasicMetadataValueEnum> = stack
                        .split_off(stack.len() - argc)
                        .into_iter()
                        .map(Into::into)
                        .collect();
                    let b = &self.builder;
                    let call = b.build_call(callee, &args, "")?;
                    if let Some(v) = call.try_as_basic_value().basic() {
                        if v.is_struct_value() {
                            let agg = v.into_struct_value();
                            for i in 0..agg.get_type().count_fields() {
                                stack.push(b.build_extract_value(agg, i, "")?);
                            }
                        } else {
                            stack.push(v);
                        }
                    }
                }
                other => bail!("bytecode {other:?} not supported yet"),
            }
        }
        if !terminated {
            self.builder.build_unreachable()?;
        }
        Ok(())
    }

    /// Branch to `move_rt_arith_error` if `fail` is true; continue in a fresh block otherwise.
    fn check(&self, f: FunctionValue<'ctx>, fail: IntValue<'ctx>) -> Result<()> {
        let ok = self.ctx.append_basic_block(f, "ok");
        let bad = self.ctx.append_basic_block(f, "arith_error");
        self.builder.build_conditional_branch(fail, bad, ok)?;
        self.builder.position_at_end(bad);
        self.builder.build_call(self.arith_error(), &[], "")?;
        self.builder.build_unreachable()?;
        self.builder.position_at_end(ok);
        Ok(())
    }
}

/// Little-endian 64-bit words of `2^to_bits - 1`, sized for a `from_bits`-wide constant.
fn max_words(to_bits: u32, from_bits: u32) -> Vec<u64> {
    let n = (from_bits as usize).div_ceil(64);
    (0..n)
        .map(|i| {
            let lo = i as u32 * 64;
            if to_bits >= lo + 64 {
                u64::MAX
            } else if to_bits > lo {
                (1u64 << (to_bits - lo)) - 1
            } else {
                0
            }
        })
        .collect()
}
