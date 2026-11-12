//! Monomorphic Move types.
//!
//! Bytecode signatures (`SignatureToken`) may mention type parameters; every `Type` here
//! is fully instantiated — generics are compiled by monomorphization, so each generic
//! function/struct gets one LLVM definition per concrete type argument list.

use std::fmt::Write;

use anyhow::{Result, anyhow, bail};
use move_binary_format::file_format::SignatureToken;

use move_core_types::{
    account_address::AccountAddress,
    identifier::Identifier,
    language_storage::{StructTag, TypeTag},
};

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

impl Type {
    /// Sui's canonical type string (what `std::type_name` returns): addresses as 64 hex
    /// digits without `0x`, type arguments separated by `,` without spaces, e.g.
    /// `0000…0002::coin::Coin<0000…0002::sui::SUI>`.
    /// Types of upgraded packages are named by their defining address (see `TypeOrigin`).
    pub fn canonical(&self, prog: &Program) -> String {
        self.canonical_with(prog, true)
    }

    /// `defining`: name types by their defining address (Sui type tags, `with_defining_ids`),
    /// or by their package's original address (`with_original_ids`).
    pub fn canonical_with(&self, prog: &Program, defining: bool) -> String {
        let datatype = |m: ModuleIdx, name: String, args: &[Type]| {
            let cm = prog.module(m);
            let args = args
                .iter()
                .map(|a| a.canonical_with(prog, defining))
                .collect::<Vec<_>>();
            let args = if args.is_empty() {
                String::new()
            } else {
                format!("<{}>", args.join(","))
            };
            let name = name.rsplit("::").next().unwrap().to_string();
            let addr = if defining {
                prog.defining_address(m, &name)
            } else {
                *cm.address()
            };
            format!(
                "{}::{}::{}{args}",
                addr.to_canonical_string(false),
                cm.name(),
                name
            )
        };
        match self {
            Type::Vector(e) => format!("vector<{}>", e.canonical_with(prog, defining)),
            Type::Struct(id, args) => datatype(id.module, prog.struct_name(*id), args),
            Type::Enum(id, args) => datatype(id.module, prog.enum_name(*id), args),
            Type::Ref(t) => t.canonical_with(prog, defining),
            scalar => scalar.display(prog),
        }
    }

    /// Parse a type in Move/Sui syntax: `u64`, `vector<u8>`, `0x2::coin::Coin<0x…::usdc::USDC>`
    /// (addresses with or without `0x`, short or 64 hex digits).
    pub fn parse(prog: &Program, s: &str) -> Result<Type> {
        let s = s.trim();
        let (head, args) = match s.find('<') {
            Some(i) => {
                let inner = s[i + 1..]
                    .strip_suffix('>')
                    .ok_or_else(|| anyhow!("bad type {s}"))?;
                (
                    &s[..i],
                    split_type_args(inner)
                        .iter()
                        .map(|a| Self::parse(prog, a))
                        .collect::<Result<Vec<_>>>()?,
                )
            }
            None => (s, vec![]),
        };
        Ok(match (head, args.as_slice()) {
            ("bool", []) => Type::Bool,
            ("u8", []) => Type::U8,
            ("u16", []) => Type::U16,
            ("u32", []) => Type::U32,
            ("u64", []) => Type::U64,
            ("u128", []) => Type::U128,
            ("u256", []) => Type::U256,
            ("address", []) => Type::Address,
            ("vector", [e]) => Type::Vector(Box::new(e.clone())),
            _ => {
                let parts: Vec<&str> = head.split("::").collect();
                let [addr, module, name] = parts.as_slice() else {
                    bail!("bad type {s}")
                };
                let addr = AccountAddress::from_hex_literal(&format!(
                    "0x{}",
                    addr.trim_start_matches("0x")
                ))
                .map_err(|e| anyhow!("bad address in {s}: {e}"))?;
                let d = prog
                    .datatype_by_name(&addr, module, name)
                    .or_else(|| prog.datatype_by_defining(&addr, module, name))
                    .ok_or_else(|| anyhow!("type {s} not loaded"))?;
                Self::datatype(d, args)
            }
        })
    }

    /// The Move `TypeTag` of this type (as used by Sui to derive dynamic field IDs).
    pub fn type_tag(&self, prog: &Program) -> Result<TypeTag> {
        let datatype = |m: ModuleIdx, name: String, args: &[Type]| -> Result<TypeTag> {
            let cm = prog.module(m);
            let short = name.rsplit("::").next().unwrap();
            Ok(TypeTag::Struct(Box::new(StructTag {
                address: prog.defining_address(m, short),
                module: cm.name().to_owned(),
                name: Identifier::new(name.rsplit("::").next().unwrap())?,
                type_params: args
                    .iter()
                    .map(|a| a.type_tag(prog))
                    .collect::<Result<_>>()?,
            })))
        };
        Ok(match self {
            Type::Bool => TypeTag::Bool,
            Type::U8 => TypeTag::U8,
            Type::U16 => TypeTag::U16,
            Type::U32 => TypeTag::U32,
            Type::U64 => TypeTag::U64,
            Type::U128 => TypeTag::U128,
            Type::U256 => TypeTag::U256,
            Type::Address => TypeTag::Address,
            Type::Vector(e) => TypeTag::Vector(Box::new(e.type_tag(prog)?)),
            Type::Struct(id, args) => datatype(id.module, prog.struct_name(*id), args)?,
            Type::Enum(id, args) => datatype(id.module, prog.enum_name(*id), args)?,
            Type::Ref(_) => bail!("references have no type tag"),
        })
    }

    /// For a struct/enum: its defining address (`defining`) or its package's original address.
    pub fn datatype_address(&self, prog: &Program, defining: bool) -> Option<AccountAddress> {
        let (m, name) = match self {
            Type::Struct(id, _) => (id.module, prog.struct_name(*id)),
            Type::Enum(id, _) => (id.module, prog.enum_name(*id)),
            _ => return None,
        };
        let short = name.rsplit("::").next().unwrap();
        Some(if defining {
            prog.defining_address(m, short)
        } else {
            *prog.module(m).address()
        })
    }

    /// Module defining a struct/enum type.
    pub fn defining_module(&self) -> Option<ModuleIdx> {
        match self {
            Type::Struct(id, _) => Some(id.module),
            Type::Enum(id, _) => Some(id.module),
            _ => None,
        }
    }
}

/// Split `A, B<C, D>, E` at top-level commas.
pub fn split_type_args(s: &str) -> Vec<&str> {
    let (mut depth, mut start, mut out) = (0, 0, vec![]);
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
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
