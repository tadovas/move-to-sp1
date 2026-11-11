//! Type descriptors and in-memory value layout shared with the compiler.
//!
//! The compiler (`compiler/src/translate/descriptor.rs`) emits one `TypeDesc` per Move type
//! that crosses into the runtime. Layouts here must match it field for field.
//!
//! Value layout in memory (what a `*const u8` value pointer points at):
//! * bool: one byte, 0 or 1; integers: little-endian, `size` bytes; address: 32 raw bytes
//! * vector: a [`VecHeader`]; elements are `elem.size` bytes apart
//! * struct: fields at `FieldDesc::offset`
//! * enum: `u16` tag at offset 0, variant fields at `payload_offset + FieldDesc::offset`

use core::{slice, str};

pub const BOOL: u64 = 0;
pub const U8: u64 = 1;
pub const U16: u64 = 2;
pub const U32: u64 = 3;
pub const U64: u64 = 4;
pub const U128: u64 = 5;
pub const U256: u64 = 6;
pub const ADDRESS: u64 = 7;
pub const VECTOR: u64 = 8;
pub const STRUCT: u64 = 9;
pub const ENUM: u64 = 10;

#[repr(C)]
pub struct TypeDesc {
    pub kind: u64,
    /// Size of the in-memory value (ABI size, i.e. the stride in a vector).
    pub size: u64,
    name: *const u8,
    name_len: u64,
    /// Element type (vectors only).
    elem: *const TypeDesc,
    fields: *const FieldDesc,
    n_fields: u64,
    variants: *const VariantDesc,
    n_variants: u64,
    /// Offset of the variant payload (enums only).
    pub payload_offset: u64,
}

#[repr(C)]
pub struct FieldDesc {
    name: *const u8,
    name_len: u64,
    pub offset: u64,
    ty: *const TypeDesc,
}

#[repr(C)]
pub struct VariantDesc {
    name: *const u8,
    name_len: u64,
    fields: *const FieldDesc,
    n_fields: u64,
}

/// The `{data, len, cap}` header of a Move vector.
#[repr(C)]
pub struct VecHeader {
    pub data: *mut u8,
    pub len: u64,
    pub cap: u64,
}

unsafe fn text<'a>(p: *const u8, n: u64) -> &'a str {
    unsafe { str::from_utf8_unchecked(slice::from_raw_parts(p, n as usize)) }
}

unsafe fn items<'a, T>(p: *const T, n: u64) -> &'a [T] {
    if n == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(p, n as usize) }
    }
}

impl TypeDesc {
    /// Move type name, e.g. `pool::Pool` or `vector<u8>`.
    pub fn name(&self) -> &str {
        unsafe { text(self.name, self.name_len) }
    }
    pub fn elem(&self) -> &TypeDesc {
        unsafe { &*self.elem }
    }
    pub fn fields(&self) -> &[FieldDesc] {
        unsafe { items(self.fields, self.n_fields) }
    }
    pub fn variants(&self) -> &[VariantDesc] {
        unsafe { items(self.variants, self.n_variants) }
    }
}

impl FieldDesc {
    pub fn name(&self) -> &str {
        unsafe { text(self.name, self.name_len) }
    }
    pub fn ty(&self) -> &TypeDesc {
        unsafe { &*self.ty }
    }
}

impl VariantDesc {
    pub fn name(&self) -> &str {
        unsafe { text(self.name, self.name_len) }
    }
    pub fn fields(&self) -> &[FieldDesc] {
        unsafe { items(self.fields, self.n_fields) }
    }
}

/// Allocation used for all Move heap memory: 16-byte aligned, never freed.
pub fn alloc(size: usize) -> *mut u8 {
    let layout = core::alloc::Layout::from_size_align(size.max(1), 16).unwrap();
    unsafe { alloc::alloc::alloc(layout) }
}

impl VecHeader {
    pub unsafe fn bytes<'a>(v: *const VecHeader) -> &'a [u8] {
        let v = unsafe { &*v };
        unsafe { items(v.data, v.len) }
    }

    /// A fresh `vector<u8>` holding `bytes`.
    pub fn from_bytes(bytes: &[u8]) -> VecHeader {
        let data = alloc(bytes.len());
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len()) };
        VecHeader {
            data,
            len: bytes.len() as u64,
            cap: bytes.len() as u64,
        }
    }
}
