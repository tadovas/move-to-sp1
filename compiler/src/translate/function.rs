//! Translation of one function instance: basic blocks, locals, the operand stack and the
//! per-instruction dispatch.
//!
//! Locals live in `alloca`s (promoted to SSA by LLVM). The operand stack exists only at
//! compile time: the bytecode verifier guarantees it is empty at basic-block boundaries, so
//! within a block it is a vector of `(value, type)` pairs.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Result, anyhow, bail, ensure};
use inkwell::{
    basic_block::BasicBlock,
    values::{BasicMetadataValueEnum, BasicValueEnum, PointerValue},
};
use move_binary_format::file_format::{
    Bytecode, FieldHandleIndex, JumpTableInner, SignatureIndex, StructDefinitionIndex,
    VariantHandleIndex, VariantInstantiationHandleIndex,
};

use super::{
    FnInst, Translator,
    program::{EnumId, ModuleIdx, StructId},
    types::{Type, struct_fields},
};

/// A value on the operand stack, with its Move type.
pub(super) type Slot<'ctx> = (BasicValueEnum<'ctx>, Type);

/// Per-function translation state.
pub(super) struct Frame<'ctx> {
    /// Module the function is defined in (handles are resolved against it).
    pub m: ModuleIdx,
    /// Type arguments of this instance.
    pub subst: Vec<Type>,
    returns: Vec<Type>,
    locals: Vec<(PointerValue<'ctx>, Type)>,
    /// `VariantSwitch` jump tables: target pc per variant tag.
    jump_tables: Vec<Vec<usize>>,
    pub stack: Vec<Slot<'ctx>>,
}

impl<'ctx> Frame<'ctx> {
    pub fn pop(&mut self) -> Result<Slot<'ctx>> {
        self.stack
            .pop()
            .ok_or_else(|| anyhow!("operand stack underflow"))
    }
    pub fn pop_n(&mut self, n: usize) -> Result<Vec<Slot<'ctx>>> {
        ensure!(self.stack.len() >= n, "operand stack underflow");
        Ok(self.stack.split_off(self.stack.len() - n))
    }
    pub fn push(&mut self, v: impl Into<BasicValueEnum<'ctx>>, ty: Type) {
        self.stack.push((v.into(), ty));
    }
}

impl<'ctx> Translator<'ctx, '_> {
    pub(super) fn translate_function(&mut self, inst: &FnInst) -> Result<()> {
        let prog = self.prog;
        let cm = prog.module(inst.id.module);
        let def = cm.function_def_at(inst.id.def);
        let code = def
            .code
            .as_ref()
            .ok_or_else(|| anyhow!("native function called as Move code"))?;
        let f = self.functions[inst];
        let (params, returns) = self.signature(inst)?;
        let declared_locals = Type::from_tokens(
            prog,
            inst.id.module,
            &cm.signature_at(code.locals).0,
            &inst.type_args,
        )?;

        let jump_tables: Vec<Vec<usize>> = code
            .jump_tables
            .iter()
            .map(|jt| {
                let JumpTableInner::Full(offsets) = &jt.jump_table;
                offsets.iter().map(|o| *o as usize).collect()
            })
            .collect();

        // Basic blocks start at 0, at branch targets and after terminators.
        let n = code.code.len();
        let mut leaders = BTreeSet::from([0usize]);
        for (pc, ins) in code.code.iter().enumerate() {
            match ins {
                Bytecode::BrTrue(t) | Bytecode::BrFalse(t) | Bytecode::Branch(t) => {
                    leaders.extend([*t as usize, pc + 1]);
                }
                Bytecode::Ret | Bytecode::Abort => {
                    leaders.insert(pc + 1);
                }
                Bytecode::VariantSwitch(jt) => {
                    leaders.extend(jump_tables[jt.0 as usize].iter().copied());
                    leaders.insert(pc + 1);
                }
                _ => {}
            }
        }
        leaders.retain(|&l| l < n);

        let entry = self.ctx.append_basic_block(f, "entry");
        let blocks: HashMap<usize, BasicBlock> = leaders
            .iter()
            .map(|&pc| (pc, self.ctx.append_basic_block(f, &format!("pc{pc}"))))
            .collect();

        self.builder.position_at_end(entry);
        let mut locals = vec![];
        for (i, ty) in params.iter().chain(&declared_locals).enumerate() {
            let slot = self
                .builder
                .build_alloca(self.llvm_type(ty)?, &format!("l{i}"))?;
            if i < params.len() {
                self.builder
                    .build_store(slot, f.get_nth_param(i as u32).unwrap())?;
            }
            locals.push((slot, ty.clone()));
        }
        self.builder.build_unconditional_branch(blocks[&0])?;

        let mut frame = Frame {
            m: inst.id.module,
            subst: inst.type_args.clone(),
            returns,
            locals,
            jump_tables,
            stack: vec![],
        };
        let mut open = false; // does the current block still need a terminator?
        for (pc, ins) in code.code.iter().enumerate() {
            if let Some(bb) = blocks.get(&pc) {
                if open {
                    self.builder.build_unconditional_branch(*bb)?;
                }
                ensure!(
                    frame.stack.is_empty(),
                    "operand stack not empty at block start (pc {pc})"
                );
                self.builder.position_at_end(*bb);
            } else if !open {
                continue; // dead code after a terminator
            }
            let ends_block = self
                .instruction(&mut frame, ins, pc, &blocks)
                .map_err(|e| anyhow!("pc {pc} ({ins:?}): {e:#}"))?;
            open = !ends_block;
        }
        if open {
            self.builder.build_unreachable()?;
        }
        Ok(())
    }

    /// Translate one instruction. Returns true if it terminated the block.
    fn instruction(
        &mut self,
        fr: &mut Frame<'ctx>,
        ins: &Bytecode,
        pc: usize,
        blocks: &HashMap<usize, BasicBlock<'ctx>>,
    ) -> Result<bool> {
        use Bytecode as B;
        let b = &self.builder;
        match ins {
            B::Nop => {}
            B::Pop => {
                fr.pop()?;
            }

            // ---- constants
            B::LdTrue => fr.push(self.ctx.bool_type().const_int(1, false), Type::Bool),
            B::LdFalse => fr.push(self.ctx.bool_type().const_zero(), Type::Bool),
            B::LdU8(v) => fr.push(self.ctx.i8_type().const_int(*v as u64, false), Type::U8),
            B::LdU16(v) => fr.push(self.ctx.i16_type().const_int(*v as u64, false), Type::U16),
            B::LdU32(v) => fr.push(self.ctx.i32_type().const_int(*v as u64, false), Type::U32),
            B::LdU64(v) => fr.push(self.ctx.i64_type().const_int(*v, false), Type::U64),
            B::LdU128(v) => fr.push(self.const_int_le(128, &v.to_le_bytes()), Type::U128),
            B::LdU256(v) => fr.push(self.const_int_le(256, &v.to_le_bytes()), Type::U256),
            B::LdConst(idx) => {
                let c = self.prog.module(fr.m).constant_at(*idx);
                let ty = Type::from_token(self.prog, fr.m, &c.type_, &[])?;
                let v = self.constant(&ty, &c.data)?;
                fr.push(v, ty);
            }

            // ---- locals and references
            B::CopyLoc(i) => {
                let (slot, ty) = fr.locals[*i as usize].clone();
                let v = self.builder.build_load(self.llvm_type(&ty)?, slot, "")?;
                let v = self.copy_value(&ty, v)?;
                fr.push(v, ty);
            }
            B::MoveLoc(i) => {
                let (slot, ty) = fr.locals[*i as usize].clone();
                let v = self.builder.build_load(self.llvm_type(&ty)?, slot, "")?;
                fr.push(v, ty);
            }
            B::StLoc(i) => {
                let (v, _) = fr.pop()?;
                b.build_store(fr.locals[*i as usize].0, v)?;
            }
            B::MutBorrowLoc(i) | B::ImmBorrowLoc(i) => {
                let (slot, ty) = fr.locals[*i as usize].clone();
                fr.push(slot, Type::Ref(Box::new(ty)));
            }
            B::ReadRef => {
                let (r, ty) = fr.pop()?;
                let inner = deref(ty)?;
                let v =
                    self.builder
                        .build_load(self.llvm_type(&inner)?, r.into_pointer_value(), "")?;
                let v = self.copy_value(&inner, v)?;
                fr.push(v, inner);
            }
            B::WriteRef => {
                let (r, _) = fr.pop()?;
                let (v, _) = fr.pop()?;
                b.build_store(r.into_pointer_value(), v)?;
            }
            B::FreezeRef => {}

            // ---- structs
            B::Pack(def) => self.pack(fr, *def, None)?,
            B::PackGeneric(i) => {
                let inst = self.prog.module(fr.m).struct_instantiation_at(*i);
                self.pack(fr, inst.def, Some(inst.type_parameters))?
            }
            B::Unpack(def) => self.unpack(fr, *def, None)?,
            B::UnpackGeneric(i) => {
                let inst = self.prog.module(fr.m).struct_instantiation_at(*i);
                self.unpack(fr, inst.def, Some(inst.type_parameters))?
            }
            B::MutBorrowField(h) | B::ImmBorrowField(h) => self.borrow_field(fr, *h, None)?,
            B::MutBorrowFieldGeneric(i) | B::ImmBorrowFieldGeneric(i) => {
                let inst = self.prog.module(fr.m).field_instantiation_at(*i);
                self.borrow_field(fr, inst.handle, Some(inst.type_parameters))?
            }

            // ---- vectors
            B::VecPack(sig, n) => {
                let elem = self.sig_type(fr, *sig)?;
                let elems: Vec<_> = fr.pop_n(*n as usize)?.into_iter().map(|(v, _)| v).collect();
                let v = self.vec_from_values(&elem, &elems)?;
                fr.push(v, Type::Vector(Box::new(elem)));
            }
            B::VecUnpack(sig, n) => {
                let elem = self.sig_type(fr, *sig)?;
                let (v, _) = fr.pop()?;
                for e in self.vec_unpack(&elem, v.into_struct_value(), *n)? {
                    fr.push(e, elem.clone());
                }
            }
            B::VecLen(_) => {
                let (r, _) = fr.pop()?;
                let len = self.vec_len(r.into_pointer_value())?;
                fr.push(len, Type::U64);
            }
            B::VecImmBorrow(sig) | B::VecMutBorrow(sig) => {
                let elem = self.sig_type(fr, *sig)?;
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                let p = self.vec_elem_ptr(&elem, r.into_pointer_value(), i.into_int_value())?;
                fr.push(p, Type::Ref(Box::new(elem)));
            }
            B::VecPushBack(sig) => {
                let elem = self.sig_type(fr, *sig)?;
                let (v, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                self.vec_push(&elem, r.into_pointer_value(), v)?;
            }
            B::VecPopBack(sig) => {
                let elem = self.sig_type(fr, *sig)?;
                let (r, _) = fr.pop()?;
                let v = self.vec_pop(&elem, r.into_pointer_value())?;
                fr.push(v, elem);
            }
            B::VecSwap(sig) => {
                let elem = self.sig_type(fr, *sig)?;
                let (j, _) = fr.pop()?;
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                self.vec_swap(
                    &elem,
                    r.into_pointer_value(),
                    i.into_int_value(),
                    j.into_int_value(),
                )?;
            }

            // ---- arithmetic, logic, comparison
            B::Add
            | B::Sub
            | B::Mul
            | B::Div
            | B::Mod
            | B::BitOr
            | B::BitAnd
            | B::Xor
            | B::Or
            | B::And
            | B::Shl
            | B::Shr => {
                let (r, _) = fr.pop()?;
                let (l, ty) = fr.pop()?;
                let v = self.binary_op(ins, l.into_int_value(), r.into_int_value())?;
                fr.push(v, ty);
            }
            B::Lt | B::Gt | B::Le | B::Ge => {
                let (r, _) = fr.pop()?;
                let (l, _) = fr.pop()?;
                let v = self.compare(ins, l.into_int_value(), r.into_int_value())?;
                fr.push(v, Type::Bool);
            }
            B::Eq | B::Neq => {
                let (r, _) = fr.pop()?;
                let (l, ty) = fr.pop()?;
                let mut v = self.equals(&ty, l, r)?;
                if matches!(ins, B::Neq) {
                    v = self.builder.build_not(v, "")?;
                }
                fr.push(v, Type::Bool);
            }
            B::Not => {
                let (v, ty) = fr.pop()?;
                fr.push(b.build_not(v.into_int_value(), "")?, ty);
            }
            B::CastU8 | B::CastU16 | B::CastU32 | B::CastU64 | B::CastU128 | B::CastU256 => {
                let to = match ins {
                    B::CastU8 => Type::U8,
                    B::CastU16 => Type::U16,
                    B::CastU32 => Type::U32,
                    B::CastU64 => Type::U64,
                    B::CastU128 => Type::U128,
                    _ => Type::U256,
                };
                let (v, _) = fr.pop()?;
                let v = self.cast(v.into_int_value(), to.int_bits().unwrap())?;
                fr.push(v, to);
            }

            // ---- control flow
            B::Branch(t) => {
                b.build_unconditional_branch(blocks[&(*t as usize)])?;
                return Ok(true);
            }
            B::BrTrue(t) | B::BrFalse(t) => {
                let (c, _) = fr.pop()?;
                let (taken, fallthrough) = (blocks[&(*t as usize)], blocks[&(pc + 1)]);
                let (then, els) = if matches!(ins, B::BrTrue(_)) {
                    (taken, fallthrough)
                } else {
                    (fallthrough, taken)
                };
                b.build_conditional_branch(c.into_int_value(), then, els)?;
                return Ok(true);
            }
            B::Ret => {
                let vals: Vec<BasicValueEnum> = fr
                    .pop_n(fr.returns.len())?
                    .into_iter()
                    .map(|(v, _)| v)
                    .collect();
                match vals.as_slice() {
                    [] => self.builder.build_return(None)?,
                    [v] => self.builder.build_return(Some(v))?,
                    _ => self.builder.build_aggregate_return(&vals)?,
                };
                return Ok(true);
            }
            B::Abort => {
                let (code, _) = fr.pop()?;
                self.abort(code.into_int_value())?;
                fr.stack.clear();
                return Ok(true);
            }

            // ---- calls
            B::Call(h) => {
                let id = self.prog.resolve_function(fr.m, *h)?;
                self.call(
                    fr,
                    FnInst {
                        id,
                        type_args: vec![],
                    },
                )?;
            }
            B::CallGeneric(i) => {
                let fi = self.prog.module(fr.m).function_instantiation_at(*i);
                let id = self.prog.resolve_function(fr.m, fi.handle)?;
                let type_args = self.sig_types(fr, fi.type_parameters)?;
                self.call(fr, FnInst { id, type_args })?;
            }

            // ---- enums
            B::PackVariant(h) => {
                let (ty, tag) = self.variant(fr, *h)?;
                self.pack_variant_op(fr, &ty, tag)?
            }
            B::PackVariantGeneric(h) => {
                let (ty, tag) = self.variant_generic(fr, *h)?;
                self.pack_variant_op(fr, &ty, tag)?
            }
            B::UnpackVariant(h) => {
                let (ty, tag) = self.variant(fr, *h)?;
                self.unpack_variant_op(fr, &ty, tag)?
            }
            B::UnpackVariantGeneric(h) => {
                let (ty, tag) = self.variant_generic(fr, *h)?;
                self.unpack_variant_op(fr, &ty, tag)?
            }
            B::UnpackVariantImmRef(h) | B::UnpackVariantMutRef(h) => {
                let (ty, tag) = self.variant(fr, *h)?;
                self.unpack_variant_ref_op(fr, &ty, tag)?
            }
            B::UnpackVariantGenericImmRef(h) | B::UnpackVariantGenericMutRef(h) => {
                let (ty, tag) = self.variant_generic(fr, *h)?;
                self.unpack_variant_ref_op(fr, &ty, tag)?
            }
            B::VariantSwitch(jt) => {
                let (r, ty) = fr.pop()?;
                let targets: Vec<_> = fr.jump_tables[jt.0 as usize]
                    .iter()
                    .map(|pc| blocks[pc])
                    .collect();
                self.variant_switch(&deref(ty)?, r.into_pointer_value(), &targets)?;
                return Ok(true);
            }

            other => bail!("instruction not supported yet: {other:?}"),
        }
        Ok(false)
    }

    fn call(&mut self, fr: &mut Frame<'ctx>, callee: FnInst) -> Result<()> {
        if self.prog.is_native(callee.id) {
            return self.call_native(fr, &callee);
        }
        let f = self.function(&callee)?;
        let (params, returns) = self.signature(&callee)?;
        let args: Vec<BasicMetadataValueEnum> = fr
            .pop_n(params.len())?
            .into_iter()
            .map(|(v, _)| v.into())
            .collect();
        let call = self.builder.build_call(f, &args, "")?;
        match returns.len() {
            0 => {}
            1 => fr.push(
                call.try_as_basic_value().basic().unwrap(),
                returns[0].clone(),
            ),
            _ => {
                let agg = call
                    .try_as_basic_value()
                    .basic()
                    .unwrap()
                    .into_struct_value();
                for (i, ty) in returns.into_iter().enumerate() {
                    let v = self.builder.build_extract_value(agg, i as u32, "")?;
                    fr.push(v, ty);
                }
            }
        }
        Ok(())
    }

    // ---- structs

    fn struct_type_of(
        &self,
        fr: &Frame,
        def: StructDefinitionIndex,
        inst: Option<SignatureIndex>,
    ) -> Result<Type> {
        let id = StructId { module: fr.m, def };
        let args = match inst {
            Some(sig) => self.sig_types(fr, sig)?,
            None => vec![],
        };
        Ok(Type::Struct(id, args))
    }

    fn pack(
        &mut self,
        fr: &mut Frame<'ctx>,
        def: StructDefinitionIndex,
        inst: Option<SignatureIndex>,
    ) -> Result<()> {
        let ty = self.struct_type_of(fr, def, inst)?;
        let Type::Struct(id, args) = &ty else {
            unreachable!()
        };
        let n = struct_fields(self.prog, *id, args)?.len();
        let st = self.llvm_type(&ty)?.into_struct_type();
        let mut agg = st.get_undef();
        for (i, (v, _)) in fr.pop_n(n)?.into_iter().enumerate() {
            agg = self
                .builder
                .build_insert_value(agg, v, i as u32, "")?
                .into_struct_value();
        }
        fr.push(agg, ty);
        Ok(())
    }

    fn unpack(
        &mut self,
        fr: &mut Frame<'ctx>,
        def: StructDefinitionIndex,
        inst: Option<SignatureIndex>,
    ) -> Result<()> {
        let ty = self.struct_type_of(fr, def, inst)?;
        let Type::Struct(id, args) = &ty else {
            unreachable!()
        };
        let fields = struct_fields(self.prog, *id, args)?;
        let (v, _) = fr.pop()?;
        for (i, fty) in fields.into_iter().enumerate() {
            let f = self
                .builder
                .build_extract_value(v.into_struct_value(), i as u32, "")?;
            fr.push(f, fty);
        }
        Ok(())
    }

    fn borrow_field(
        &mut self,
        fr: &mut Frame<'ctx>,
        h: FieldHandleIndex,
        inst: Option<SignatureIndex>,
    ) -> Result<()> {
        let fh = self.prog.module(fr.m).field_handle_at(h);
        let ty = self.struct_type_of(fr, fh.owner, inst)?;
        let Type::Struct(id, args) = &ty else {
            unreachable!()
        };
        let fty = struct_fields(self.prog, *id, args)?.swap_remove(fh.field as usize);
        let st = self.llvm_type(&ty)?.into_struct_type();
        let (r, _) = fr.pop()?;
        let p =
            self.builder
                .build_struct_gep(st, r.into_pointer_value(), fh.field as u32, "field")?;
        fr.push(p, Type::Ref(Box::new(fty)));
        Ok(())
    }

    // ---- enums

    /// Enum type and tag of a (non-generic) variant handle.
    fn variant(&self, fr: &Frame, h: VariantHandleIndex) -> Result<(Type, u16)> {
        let vh = self.prog.module(fr.m).variant_handle_at(h);
        Ok((
            Type::Enum(
                EnumId {
                    module: fr.m,
                    def: vh.enum_def,
                },
                vec![],
            ),
            vh.variant,
        ))
    }

    fn variant_generic(
        &self,
        fr: &Frame,
        h: VariantInstantiationHandleIndex,
    ) -> Result<(Type, u16)> {
        let cm = self.prog.module(fr.m);
        let vh = cm.variant_instantiation_handle_at(h);
        let inst = cm.enum_instantiation_at(vh.enum_def);
        let args = self.sig_types(fr, inst.type_parameters)?;
        Ok((
            Type::Enum(
                EnumId {
                    module: fr.m,
                    def: inst.def,
                },
                args,
            ),
            vh.variant,
        ))
    }

    fn pack_variant_op(&mut self, fr: &mut Frame<'ctx>, ty: &Type, tag: u16) -> Result<()> {
        let n = self.variant_struct(ty, tag)?.1.len();
        let fields: Vec<_> = fr.pop_n(n)?.into_iter().map(|(v, _)| v).collect();
        let v = self.pack_variant(ty, tag, &fields)?;
        fr.push(v, ty.clone());
        Ok(())
    }

    fn unpack_variant_op(&mut self, fr: &mut Frame<'ctx>, ty: &Type, tag: u16) -> Result<()> {
        let (v, _) = fr.pop()?;
        for (f, fty) in self.unpack_variant(ty, tag, v)? {
            fr.push(f, fty);
        }
        Ok(())
    }

    fn unpack_variant_ref_op(&mut self, fr: &mut Frame<'ctx>, ty: &Type, tag: u16) -> Result<()> {
        let (r, _) = fr.pop()?;
        for (p, fty) in self.variant_field_ptrs(ty, r.into_pointer_value(), tag)? {
            fr.push(p, Type::Ref(Box::new(fty)));
        }
        Ok(())
    }

    // ---- helpers

    /// The single type of a signature (used by vector instructions for the element type).
    fn sig_type(&self, fr: &Frame, sig: SignatureIndex) -> Result<Type> {
        let mut tys = self.sig_types(fr, sig)?;
        ensure!(tys.len() == 1, "expected a single-type signature");
        Ok(tys.remove(0))
    }

    fn sig_types(&self, fr: &Frame, sig: SignatureIndex) -> Result<Vec<Type>> {
        Type::from_tokens(
            self.prog,
            fr.m,
            &self.prog.module(fr.m).signature_at(sig).0,
            &fr.subst,
        )
    }
}

fn deref(ty: Type) -> Result<Type> {
    match ty {
        Type::Ref(t) => Ok(*t),
        other => bail!("expected a reference, found {other:?}"),
    }
}
