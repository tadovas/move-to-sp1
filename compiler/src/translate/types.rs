//! Monomorphic Move types.
//!
//! Bytecode signatures (`SignatureToken`) may mention type parameters; every `Type` here
//! is fully instantiated — generics are compiled by monomorphization, so each generic
//! function/struct gets one LLVM definition per concrete type argument list.

use std::fmt::Write;

use anyhow::{Result, anyhow, bail};
use move_binary_format::file_format::SignatureToken;

use super::program::{Datatype, EnumId, ModuleIdx, Program, StructId};

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Type {
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Address,
    Vector(Box<Type>),
    Struct(StructId, Vec<Type>),
    Enum(EnumId, Vec<Type>),
    /// `&T` and `&mut T` (mutability doesn't matter after verification).
    Ref(Box<Type>),
}

impl Type {
    /// Instantiate a signature token from module `m`, substituting type parameters.
    pub fn from_token(
        prog: &Program,
        m: ModuleIdx,
        tok: &SignatureToken,
        subst: &[Type],
    ) -> Result<Type> {
        use SignatureToken as T;
        Ok(match tok {
            T::Bool => Type::Bool,
            T::U8 => Type::U8,
            T::U16 => Type::U16,
            T::U32 => Type::U32,
            T::U64 => Type::U64,
            T::U128 => Type::U128,
            T::U256 => Type::U256,
            T::Address => Type::Address,
            T::Signer => bail!("signer is not supported"),
            T::Vector(e) => Type::Vector(Box::new(Self::from_token(prog, m, e, subst)?)),
            T::Datatype(h) => Self::datatype(prog.resolve_datatype(m, *h)?, vec![]),
            T::DatatypeInstantiation(inst) => {
                let (h, args) = &**inst;
                let args = Self::from_tokens(prog, m, args, subst)?;
                Self::datatype(prog.resolve_datatype(m, *h)?, args)
            }
            T::Reference(t) | T::MutableReference(t) => {
                Type::Ref(Box::new(Self::from_token(prog, m, t, subst)?))
            }
            T::TypeParameter(i) => subst
                .get(*i as usize)
                .cloned()
                .ok_or_else(|| anyhow!("unbound type parameter #{i}"))?,
        })
    }

    pub fn from_tokens(
        prog: &Program,
        m: ModuleIdx,
        toks: &[SignatureToken],
        subst: &[Type],
    ) -> Result<Vec<Type>> {
        toks.iter()
            .map(|t| Self::from_token(prog, m, t, subst))
            .collect()
    }

    fn datatype(d: Datatype, args: Vec<Type>) -> Type {
        match d {
            Datatype::Struct(id) => Type::Struct(id, args),
            Datatype::Enum(id) => Type::Enum(id, args),
        }
    }

    /// Bit width of integer-like types (bool = 1, address = 256).
    pub fn int_bits(&self) -> Option<u32> {
        Some(match self {
            Type::Bool => 1,
            Type::U8 => 8,
            Type::U16 => 16,
            Type::U32 => 32,
            Type::U64 => 64,
            Type::U128 => 128,
            Type::U256 | Type::Address => 256,
            _ => return None,
        })
    }

    /// Human-readable Move syntax, e.g. `vector<demo::coin::Coin<u64>>`.
    pub fn display(&self, prog: &Program) -> String {
        let mut s = String::new();
        self.write(prog, &mut s);
        s
    }

    fn write(&self, prog: &Program, out: &mut String) {
        match self {
            Type::Bool => out.push_str("bool"),
            Type::U8 => out.push_str("u8"),
            Type::U16 => out.push_str("u16"),
            Type::U32 => out.push_str("u32"),
            Type::U64 => out.push_str("u64"),
            Type::U128 => out.push_str("u128"),
            Type::U256 => out.push_str("u256"),
            Type::Address => out.push_str("address"),
            Type::Vector(e) => {
                out.push_str("vector<");
                e.write(prog, out);
                out.push('>');
            }
            Type::Struct(id, args) => {
                let _ = write!(out, "{}", prog.struct_name(*id));
                write_args(prog, args, out);
            }
            Type::Enum(id, args) => {
                let _ = write!(out, "{}", prog.enum_name(*id));
                write_args(prog, args, out);
            }
            Type::Ref(t) => {
                out.push('&');
                t.write(prog, out);
            }
        }
    }
}

/// `<T1, T2>` (nothing for an empty list).
pub fn display_args(prog: &Program, args: &[Type]) -> String {
    let mut s = String::new();
    write_args(prog, args, &mut s);
    s
}

fn write_args(prog: &Program, args: &[Type], out: &mut String) {
    if args.is_empty() {
        return;
    }
    out.push('<');
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        a.write(prog, out);
    }
    out.push('>');
}

/// Field types of an instantiated struct.
pub fn struct_fields(prog: &Program, id: StructId, args: &[Type]) -> Result<Vec<Type>> {
    let cm = prog.module(id.module);
    let fields = cm
        .struct_def_at(id.def)
        .fields()
        .ok_or_else(|| anyhow!("native struct {} not supported", prog.struct_name(id)))?;
    fields
        .iter()
        .map(|f| Type::from_token(prog, id.module, &f.signature.0, args))
        .collect()
}

/// Field types of every variant of an instantiated enum, indexed by tag.
pub fn variant_fields(prog: &Program, id: EnumId, args: &[Type]) -> Result<Vec<Vec<Type>>> {
    let cm = prog.module(id.module);
    cm.enum_def_at(id.def)
        .variants
        .iter()
        .map(|v| {
            v.fields
                .iter()
                .map(|f| Type::from_token(prog, id.module, &f.signature.0, args))
                .collect()
        })
        .collect()
}
