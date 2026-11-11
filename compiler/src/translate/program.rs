//! The set of loaded Move modules, and resolution of handles across them.
//!
//! Move bytecode refers to functions and types of *other* modules through handles
//! (module address + name + member name). `Program` turns those into definition ids.

use anyhow::{Result, anyhow, bail};
use move_binary_format::{
    CompiledModule,
    file_format::{
        DatatypeHandleIndex, EnumDefinitionIndex, FunctionDefinitionIndex, FunctionHandleIndex,
        StructDefinitionIndex,
    },
};
use move_core_types::{account_address::AccountAddress, identifier::IdentStr};

pub type ModuleIdx = usize;

/// A function definition in a loaded module.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FnId {
    pub module: ModuleIdx,
    pub def: FunctionDefinitionIndex,
}

/// A struct definition in a loaded module.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StructId {
    pub module: ModuleIdx,
    pub def: StructDefinitionIndex,
}

/// An enum definition in a loaded module.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EnumId {
    pub module: ModuleIdx,
    pub def: EnumDefinitionIndex,
}

/// What a datatype handle refers to.
pub enum Datatype {
    Struct(StructId),
    Enum(EnumId),
}

pub struct Program<'a> {
    modules: Vec<&'a CompiledModule>,
}

impl<'a> Program<'a> {
    pub fn new(modules: Vec<&'a CompiledModule>) -> Self {
        Self { modules }
    }

    pub fn module(&self, m: ModuleIdx) -> &'a CompiledModule {
        self.modules[m]
    }

    fn find_module(&self, addr: &AccountAddress, name: &IdentStr) -> Option<ModuleIdx> {
        self.modules
            .iter()
            .position(|m| m.address() == addr && m.name() == name)
    }

    fn find_function(&self, m: ModuleIdx, name: &IdentStr) -> Option<FnId> {
        let cm = self.modules[m];
        cm.function_defs()
            .iter()
            .position(|fd| cm.identifier_at(cm.function_handle_at(fd.function).name) == name)
            .map(|i| FnId {
                module: m,
                def: FunctionDefinitionIndex(i as u16),
            })
    }

    /// Resolve a function handle used in module `m` to its definition.
    pub fn resolve_function(&self, m: ModuleIdx, h: FunctionHandleIndex) -> Result<FnId> {
        let cm = self.modules[m];
        let fh = cm.function_handle_at(h);
        let mh = cm.module_handle_at(fh.module);
        let (addr, mname) = (
            cm.address_identifier_at(mh.address),
            cm.identifier_at(mh.name),
        );
        let fname = cm.identifier_at(fh.name);
        let target = self
            .find_module(addr, mname)
            .ok_or_else(|| anyhow!("module {mname} is not available"))?;
        self.find_function(target, fname)
            .ok_or_else(|| anyhow!("function {mname}::{fname} not found"))
    }

    /// Resolve a datatype handle used in module `m` to its struct or enum definition.
    pub fn resolve_datatype(&self, m: ModuleIdx, h: DatatypeHandleIndex) -> Result<Datatype> {
        let cm = self.modules[m];
        let dh = cm.datatype_handle_at(h);
        let mh = cm.module_handle_at(dh.module);
        let (addr, mname) = (
            cm.address_identifier_at(mh.address),
            cm.identifier_at(mh.name),
        );
        let name = cm.identifier_at(dh.name);
        let target = self
            .find_module(addr, mname)
            .ok_or_else(|| anyhow!("module {mname} is not available"))?;
        let tm = self.modules[target];
        let named =
            |h: DatatypeHandleIndex| tm.identifier_at(tm.datatype_handle_at(h).name) == name;
        if let Some(i) = tm.struct_defs().iter().position(|d| named(d.struct_handle)) {
            return Ok(Datatype::Struct(StructId {
                module: target,
                def: StructDefinitionIndex(i as u16),
            }));
        }
        if let Some(i) = tm.enum_defs().iter().position(|d| named(d.enum_handle)) {
            return Ok(Datatype::Enum(EnumId {
                module: target,
                def: EnumDefinitionIndex(i as u16),
            }));
        }
        bail!("datatype {mname}::{name} not found")
    }

    pub fn is_native(&self, f: FnId) -> bool {
        self.modules[f.module].function_def_at(f.def).code.is_none()
    }

    /// `module::function`
    pub fn function_name(&self, f: FnId) -> String {
        let cm = self.modules[f.module];
        let h = cm.function_handle_at(cm.function_def_at(f.def).function);
        format!("{}::{}", cm.name(), cm.identifier_at(h.name))
    }

    /// `module::Struct`
    pub fn struct_name(&self, s: StructId) -> String {
        let cm = self.modules[s.module];
        let h = cm.datatype_handle_at(cm.struct_def_at(s.def).struct_handle);
        format!("{}::{}", cm.name(), cm.identifier_at(h.name))
    }

    /// `module::Enum`
    pub fn enum_name(&self, e: EnumId) -> String {
        let cm = self.modules[e.module];
        let h = cm.datatype_handle_at(cm.enum_def_at(e.def).enum_handle);
        format!("{}::{}", cm.name(), cm.identifier_at(h.name))
    }

    /// Address of the module defining `f` (to tell `std` natives apart from user code).
    pub fn module_address(&self, m: ModuleIdx) -> &AccountAddress {
        self.modules[m].address()
    }

    /// Find the entry function: `module::function` or a bare `function` name, searched in
    /// `candidates` (the user's modules).
    pub fn lookup_entry(&self, spec: &str, candidates: &[ModuleIdx]) -> Result<FnId> {
        let (mname, fname) = match spec.rsplit_once("::") {
            Some((m, f)) => (Some(m.rsplit("::").next().unwrap()), f),
            None => (None, spec),
        };
        let fname = IdentStr::new(fname)?;
        let found: Vec<FnId> = candidates
            .iter()
            .filter(|&&m| mname.is_none_or(|n| self.modules[m].name().as_str() == n))
            .filter_map(|&m| self.find_function(m, fname))
            .collect();
        match found.as_slice() {
            [f] => Ok(*f),
            [] => bail!("entry function `{spec}` not found"),
            _ => bail!("entry function `{spec}` is ambiguous; use module::function"),
        }
    }
}
