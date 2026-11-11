//! Native functions: functions declared `native` in Move, implemented by the translator
//! (inline IR) or the runtime library.

use anyhow::{Result, bail};
use move_core_types::account_address::AccountAddress;

use super::{FnInst, Translator, function::Frame, types::Type};

impl<'ctx> Translator<'ctx, '_> {
    pub(super) fn call_native(&mut self, fr: &mut Frame<'ctx>, f: &FnInst) -> Result<()> {
        let name = self.prog.function_name(f.id);
        if self.prog.module_address(f.id.module) != &AccountAddress::ONE {
            bail!("native function {name} is not supported");
        }
        let t = |i: usize| f.type_args[i].clone();
        match name.as_str() {
            "vector::empty" => {
                let v = self.vec_from_values(&t(0), &[])?;
                fr.push(v, Type::Vector(Box::new(t(0))));
            }
            "vector::length" => {
                let (r, _) = fr.pop()?;
                let len = self.vec_len(r.into_pointer_value())?;
                fr.push(len, Type::U64);
            }
            "vector::borrow" | "vector::borrow_mut" => {
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                let p = self.vec_elem_ptr(&t(0), r.into_pointer_value(), i.into_int_value())?;
                fr.push(p, Type::Ref(Box::new(t(0))));
            }
            "vector::push_back" => {
                let (v, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                self.vec_push(&t(0), r.into_pointer_value(), v)?;
            }
            "vector::pop_back" => {
                let (r, _) = fr.pop()?;
                let v = self.vec_pop(&t(0), r.into_pointer_value())?;
                fr.push(v, t(0));
            }
            "vector::destroy_empty" => {
                let (v, _) = fr.pop()?;
                self.vec_destroy_empty(v.into_struct_value())?;
            }
            "vector::swap" => {
                let (j, _) = fr.pop()?;
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                self.vec_swap(
                    &t(0),
                    r.into_pointer_value(),
                    i.into_int_value(),
                    j.into_int_value(),
                )?;
            }
            "vector::reverse" => {
                let (r, _) = fr.pop()?;
                self.vec_reverse(&t(0), r.into_pointer_value())?;
            }
            "vector::keep_range" => {
                let (j, _) = fr.pop()?;
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                self.vec_keep_range(
                    &t(0),
                    r.into_pointer_value(),
                    i.into_int_value(),
                    j.into_int_value(),
                )?;
            }
            "vector::copy_range" => {
                let (j, _) = fr.pop()?;
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                let v = self.vec_copy_range(
                    &t(0),
                    r.into_pointer_value(),
                    i.into_int_value(),
                    j.into_int_value(),
                )?;
                fr.push(v, Type::Vector(Box::new(t(0))));
            }
            "vector::replace_range" => {
                let (other, _) = fr.pop()?;
                let (j, _) = fr.pop()?;
                let (i, _) = fr.pop()?;
                let (r, _) = fr.pop()?;
                let removed = self.vec_replace_range(
                    &t(0),
                    r.into_pointer_value(),
                    i.into_int_value(),
                    j.into_int_value(),
                    other.into_struct_value(),
                )?;
                fr.push(removed, Type::Vector(Box::new(t(0))));
            }
            _ => bail!("native function {name} is not supported yet"),
        }
        Ok(())
    }
}
