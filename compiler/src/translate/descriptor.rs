//! Type descriptors: static data describing the in-memory layout of a Move type, so the
//! runtime can walk values generically (BCS encode/decode, `debug::print`).
//!
//! Emitted as constant globals `move.desc.<type>`, one per type, on first use. The layout
//! of these structs is mirrored by `runtime/src/desc.rs` — keep the two in sync.

use anyhow::{Result, bail};
use inkwell::{
    module::Linkage,
    types::StructType,
    values::{BasicValueEnum, PointerValue, StructValue},
};

use super::{
    Translator,
    types::{Type, struct_fields, variant_fields},
};

/// `TypeDesc::kind` values (runtime/src/desc.rs).
fn kind(ty: &Type) -> Result<u64> {
    Ok(match ty {
        Type::Bool => 0,
        Type::U8 => 1,
        Type::U16 => 2,
        Type::U32 => 3,
        Type::U64 => 4,
        Type::U128 => 5,
        Type::U256 => 6,
        Type::Address => 7,
        Type::Vector(_) => 8,
        Type::Struct(..) => 9,
        Type::Enum(..) => 10,
        Type::Ref(_) => bail!("references have no type descriptor"),
    })
}

impl<'ctx> Translator<'ctx, '_> {
    /// `{ kind, size, name, name_len, elem, fields, n_fields, variants, n_variants, payload_offset }`
    fn desc_type(&self) -> StructType<'ctx> {
        self.named_struct("move.TypeDesc", |t| {
            let (i64t, ptr) = (t.ctx.i64_type().into(), t.ptr_type().into());
            vec![i64t, i64t, ptr, i64t, ptr, ptr, i64t, ptr, i64t, i64t]
        })
    }

    /// `{ name, name_len, offset, ty }`
    fn field_desc_type(&self) -> StructType<'ctx> {
        self.named_struct("move.FieldDesc", |t| {
            let (i64t, ptr) = (t.ctx.i64_type().into(), t.ptr_type().into());
            vec![ptr, i64t, i64t, ptr]
        })
    }

    /// `{ name, name_len, fields, n_fields }`
    fn variant_desc_type(&self) -> StructType<'ctx> {
        self.named_struct("move.VariantDesc", |t| {
            let (i64t, ptr) = (t.ctx.i64_type().into(), t.ptr_type().into());
            vec![ptr, i64t, ptr, i64t]
        })
    }

    fn named_struct(
        &self,
        name: &str,
        fields: impl FnOnce(&Self) -> Vec<inkwell::types::BasicTypeEnum<'ctx>>,
    ) -> StructType<'ctx> {
        if let Some(t) = self.module.get_struct_type(name) {
            return t;
        }
        let t = self.ctx.opaque_struct_type(name);
        t.set_body(&fields(self), false);
        t
    }

    /// Pointer to the descriptor of `ty`.
    pub(super) fn type_desc(&self, ty: &Type) -> Result<PointerValue<'ctx>> {
        // keyed by the canonical name: unique per type (the runtime relies on descriptor
        // identity, e.g. for dynamic-field type checks)
        let name = format!("move.desc.{}", ty.canonical(self.prog));
        if let Some(g) = self.module.get_global(&name) {
            return Ok(g.as_pointer_value());
        }
        // Declare first so recursive references (element/field descriptors) resolve.
        let g = self.module.add_global(self.desc_type(), None, &name);
        g.set_constant(true);
        g.set_linkage(Linkage::Private);

        let i64 = |v: u64| self.ctx.i64_type().const_int(v, false);
        let null = self.ptr_type().const_null();
        let lty = self.llvm_type(ty)?;
        let (name_ptr, name_len) = self.const_str(&ty.display(self.prog));
        let (mut elem, mut fields, mut n_fields) = (null, null, 0);
        let (mut variants, mut n_variants, mut payload_offset) = (null, 0, 0);
        match ty {
            Type::Vector(e) => elem = self.type_desc(e)?,
            Type::Struct(id, args) => {
                let tys = struct_fields(self.prog, *id, args)?;
                let cm = self.prog.module(id.module);
                let names: Vec<String> = cm
                    .struct_def_at(id.def)
                    .fields()
                    .unwrap()
                    .iter()
                    .map(|f| cm.identifier_at(f.name).to_string())
                    .collect();
                (fields, n_fields) =
                    self.field_descs(&name, lty.into_struct_type(), &names, &tys)?;
            }
            Type::Enum(id, args) => {
                let cm = self.prog.module(id.module);
                let def = cm.enum_def_at(id.def);
                payload_offset = self
                    .target_data
                    .offset_of_element(&lty.into_struct_type(), 1)
                    .unwrap();
                let mut vs: Vec<StructValue> = vec![];
                for (tag, tys) in variant_fields(self.prog, *id, args)?.iter().enumerate() {
                    let v = &def.variants[tag];
                    let names: Vec<String> = v
                        .fields
                        .iter()
                        .map(|f| cm.identifier_at(f.name).to_string())
                        .collect();
                    let (vst, _) = self.variant_struct(ty, tag as u16)?;
                    let (fp, n) = self.field_descs(&format!("{name}.{tag}"), vst, &names, tys)?;
                    let (vn, vl) = self.const_str(cm.identifier_at(v.variant_name).as_str());
                    vs.push(self.variant_desc_type().const_named_struct(&[
                        vn.into(),
                        i64(vl).into(),
                        fp.into(),
                        i64(n).into(),
                    ]));
                }
                n_variants = vs.len() as u64;
                variants = self.const_array_global(
                    &format!("{name}.variants"),
                    self.variant_desc_type(),
                    &vs,
                );
            }
            _ => {}
        }
        let init: [BasicValueEnum; 10] = [
            i64(kind(ty)?).into(),
            i64(self.target_data.get_abi_size(&lty)).into(),
            name_ptr.into(),
            i64(name_len).into(),
            elem.into(),
            fields.into(),
            i64(n_fields).into(),
            variants.into(),
            i64(n_variants).into(),
            i64(payload_offset).into(),
        ];
        g.set_initializer(&self.desc_type().const_named_struct(&init));
        Ok(g.as_pointer_value())
    }

    /// Field descriptor array for the fields of LLVM struct `st`.
    fn field_descs(
        &self,
        prefix: &str,
        st: StructType<'ctx>,
        names: &[String],
        tys: &[Type],
    ) -> Result<(PointerValue<'ctx>, u64)> {
        let mut fs = vec![];
        for (i, (n, t)) in names.iter().zip(tys).enumerate() {
            let (np, nl) = self.const_str(n);
            let offset = self.target_data.offset_of_element(&st, i as u32).unwrap();
            let i64 = |v: u64| self.ctx.i64_type().const_int(v, false);
            fs.push(self.field_desc_type().const_named_struct(&[
                np.into(),
                i64(nl).into(),
                i64(offset).into(),
                self.type_desc(t)?.into(),
            ]));
        }
        let p = self.const_array_global(&format!("{prefix}.fields"), self.field_desc_type(), &fs);
        Ok((p, fs.len() as u64))
    }

    fn const_array_global(
        &self,
        name: &str,
        elem: StructType<'ctx>,
        items: &[StructValue<'ctx>],
    ) -> PointerValue<'ctx> {
        if items.is_empty() {
            return self.ptr_type().const_null();
        }
        let arr = elem.const_array(items);
        let g = self.module.add_global(arr.get_type(), None, name);
        g.set_initializer(&arr);
        g.set_constant(true);
        g.set_linkage(Linkage::Private);
        g.as_pointer_value()
    }

    /// A private constant string; returns (pointer, length).
    fn const_str(&self, s: &str) -> (PointerValue<'ctx>, u64) {
        let data = self.ctx.const_string(s.as_bytes(), false);
        let g = self.module.add_global(data.get_type(), None, ".str");
        g.set_initializer(&data);
        g.set_constant(true);
        g.set_linkage(Linkage::Private);
        g.set_unnamed_addr(true);
        (g.as_pointer_value(), s.len() as u64)
    }
}
