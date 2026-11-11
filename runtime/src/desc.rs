//! Type descriptors and typed views of Move values in memory.
//!
//! The compiler (`compiler/src/translate/descriptor.rs`) emits one `TypeDesc` per Move type
//! that crosses into the runtime; the layouts here must match it field for field.
//!
//! All raw-pointer handling of Move values lives in this module. Code elsewhere works with
//! [`Value`] (read) and [`Slot`] (write): their only `unsafe` part is construction, where
//! the caller vouches that a pointer holds (or has room for) a value of the descriptor's
//! type. Navigation (fields, variants, elements) is derived from the view's own
//! descriptor and is safe.
//!
//! Value layout in memory:
//! * bool: one byte, 0 or 1; integers: little-endian; address: 32 raw bytes
//! * vector: a [`VecHeader`]; elements are `elem.size` bytes apart
//! * struct: fields at `FieldDesc::offset`
//! * enum: `u16` tag at offset 0, variant fields at `payload_offset + FieldDesc::offset`
//!
//! Descriptors themselves are trusted: they are only ever obtained as `&TypeDesc` from
//! compiler-generated constant data.

use core::{marker::PhantomData, mem::MaybeUninit, slice, str};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Address,
    Vector,
    Struct,
    Enum,
}

impl Kind {
    /// Size in bytes of a primitive (identical in memory and in BCS).
    pub fn prim_size(self) -> Option<usize> {
        Some(match self {
            Kind::Bool | Kind::U8 => 1,
            Kind::U16 => 2,
            Kind::U32 => 4,
            Kind::U64 => 8,
            Kind::U128 => 16,
            Kind::U256 | Kind::Address => 32,
            Kind::Vector | Kind::Struct | Kind::Enum => return None,
        })
    }
}

#[repr(C)]
pub struct TypeDesc {
    kind: u64,
    /// Size of the in-memory value (ABI size, i.e. the stride in a vector).
    size: u64,
    name: *const u8,
    name_len: u64,
    /// Element type (vectors only).
    elem: *const TypeDesc,
    fields: *const FieldDesc,
    n_fields: u64,
    variants: *const VariantDesc,
    n_variants: u64,
    /// Offset of the variant payload (enums only).
    payload_offset: u64,
}

#[repr(C)]
pub struct FieldDesc {
    name: *const u8,
    name_len: u64,
    offset: u64,
    ty: *const TypeDesc,
}

#[repr(C)]
pub struct VariantDesc {
    name: *const u8,
    name_len: u64,
    fields: *const FieldDesc,
    n_fields: u64,
}

/// Descriptor strings/arrays are compiler-emitted constants (see module docs).
fn text<'a>(p: *const u8, n: u64) -> &'a str {
    // SAFETY: descriptor names are valid UTF-8 constants of length `n`.
    unsafe { str::from_utf8_unchecked(slice::from_raw_parts(p, n as usize)) }
}

fn items<'a, T>(p: *const T, n: u64) -> &'a [T] {
    if n == 0 {
        return &[];
    }
    // SAFETY: non-empty descriptor arrays are constants with exactly `n` entries.
    unsafe { slice::from_raw_parts(p, n as usize) }
}

impl TypeDesc {
    pub fn kind(&self) -> Kind {
        const KINDS: [Kind; 11] = [
            Kind::Bool,
            Kind::U8,
            Kind::U16,
            Kind::U32,
            Kind::U64,
            Kind::U128,
            Kind::U256,
            Kind::Address,
            Kind::Vector,
            Kind::Struct,
            Kind::Enum,
        ];
        KINDS[self.kind as usize]
    }
    /// Move type name, e.g. `pool::Pool`.
    pub fn name(&self) -> &str {
        text(self.name, self.name_len)
    }
    /// Element type of a vector.
    pub fn elem(&self) -> &TypeDesc {
        assert_eq!(self.kind(), Kind::Vector);
        // SAFETY: vector descriptors always carry a valid element descriptor.
        unsafe { &*self.elem }
    }
    pub fn fields(&self) -> &[FieldDesc] {
        items(self.fields, self.n_fields)
    }
    pub fn variants(&self) -> &[VariantDesc] {
        items(self.variants, self.n_variants)
    }
}

impl FieldDesc {
    pub fn name(&self) -> &str {
        text(self.name, self.name_len)
    }
    pub fn ty(&self) -> &TypeDesc {
        // SAFETY: field descriptors always point at a valid type descriptor.
        unsafe { &*self.ty }
    }
}

impl VariantDesc {
    pub fn name(&self) -> &str {
        text(self.name, self.name_len)
    }
    pub fn fields(&self) -> &[FieldDesc] {
        items(self.fields, self.n_fields)
    }
}

// ---------------------------------------------------------------------------------------
// vectors

/// The `{data, len, cap}` header of a Move vector. Fields are private: a header is either
/// produced by generated code (and received through FFI) or built here, so `data` always
/// points at `len` initialized elements.
#[repr(C)]
pub struct VecHeader {
    data: *mut u8,
    len: u64,
    cap: u64,
}

impl VecHeader {
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// The contents of a `vector<u8>`.
    pub fn bytes(&self) -> &[u8] {
        items(self.data, self.len)
    }

    /// A fresh `vector<u8>` holding `bytes`.
    pub fn from_bytes(bytes: &[u8]) -> VecHeader {
        let data = alloc(bytes.len());
        // SAFETY: `data` is a fresh allocation of at least `bytes.len()` bytes.
        unsafe { slice::from_raw_parts_mut(data, bytes.len()) }.copy_from_slice(bytes);
        VecHeader {
            data,
            len: bytes.len() as u64,
            cap: bytes.len() as u64,
        }
    }
}

/// Allocation used for all Move heap memory: 16-byte aligned, never freed.
pub fn alloc(size: usize) -> *mut u8 {
    let layout = core::alloc::Layout::from_size_align(size.max(1), 16).expect("allocation size");
    // SAFETY: `layout` has non-zero size.
    let p = unsafe { alloc::alloc::alloc(layout) };
    if p.is_null() {
        alloc::alloc::handle_alloc_error(layout);
    }
    p
}

// ---------------------------------------------------------------------------------------
// read view

/// A read-only view of an initialized Move value of type `desc`.
#[derive(Clone, Copy)]
pub struct Value<'a> {
    desc: &'a TypeDesc,
    ptr: *const u8,
    _value: PhantomData<&'a u8>,
}

impl<'a> Value<'a> {
    /// # Safety
    /// `ptr` must point to an initialized value of type `desc` that stays alive and
    /// unmodified for `'a`.
    pub unsafe fn new(desc: &'a TypeDesc, ptr: *const u8) -> Self {
        Value {
            desc,
            ptr,
            _value: PhantomData,
        }
    }

    pub fn desc(&self) -> &'a TypeDesc {
        self.desc
    }

    /// The view of a sub-value at `offset` (which, by construction of the callers below,
    /// is a field/element of this value of type `desc`).
    fn at(&self, desc: &'a TypeDesc, offset: usize) -> Value<'a> {
        // SAFETY: callers only pass offsets/descriptors taken from this value's own
        // descriptor, so the sub-value is in bounds and initialized.
        Value {
            desc,
            ptr: unsafe { self.ptr.add(offset) },
            _value: PhantomData,
        }
    }

    /// Bytes of a primitive (bool / integer / address), little-endian.
    pub fn prim_bytes(&self) -> &'a [u8] {
        let n = self.desc.kind().prim_size().expect("not a primitive");
        // SAFETY: a primitive value is `n` initialized bytes.
        unsafe { slice::from_raw_parts(self.ptr, n) }
    }

    fn header(&self) -> &'a VecHeader {
        assert_eq!(self.desc.kind(), Kind::Vector);
        // SAFETY: a vector value is a `VecHeader`.
        unsafe { &*(self.ptr as *const VecHeader) }
    }

    /// Elements of a vector.
    pub fn elems(&self) -> impl ExactSizeIterator<Item = Value<'a>> + 'a {
        let (h, e) = (self.header(), self.desc.elem());
        let base = Value {
            desc: e,
            ptr: h.data,
            _value: PhantomData,
        };
        (0..h.len()).map(move |i| base.at(e, i * e.size as usize))
    }

    /// Contents of a `vector<u8>`.
    pub fn vec_bytes(&self) -> &'a [u8] {
        self.header().bytes()
    }

    /// Fields of a struct, in declaration order.
    pub fn fields(&self) -> impl Iterator<Item = (&'a FieldDesc, Value<'a>)> + 'a {
        assert_eq!(self.desc.kind(), Kind::Struct);
        let this = *self;
        self.desc
            .fields()
            .iter()
            .map(move |f| (f, this.at(f.ty(), f.offset as usize)))
    }

    /// Active variant of an enum: (tag, variant, its fields).
    pub fn variant(
        &self,
    ) -> (
        u16,
        &'a VariantDesc,
        impl Iterator<Item = (&'a FieldDesc, Value<'a>)> + 'a,
    ) {
        assert_eq!(self.desc.kind(), Kind::Enum);
        // SAFETY: an enum value starts with its `u16` tag.
        let tag = unsafe { *(self.ptr as *const u16) };
        let v = &self.desc.variants()[tag as usize];
        let payload = self.at(self.desc, self.desc.payload_offset as usize);
        (
            tag,
            v,
            v.fields()
                .iter()
                .map(move |f| (f, payload.at(f.ty(), f.offset as usize))),
        )
    }
}

// ---------------------------------------------------------------------------------------
// write view

/// A writable, possibly uninitialized slot for a Move value of type `desc`.
pub struct Slot<'a> {
    desc: &'a TypeDesc,
    ptr: *mut u8,
    _slot: PhantomData<&'a mut u8>,
}

impl<'a> Slot<'a> {
    /// # Safety
    /// `ptr` must be valid for writes of a value of type `desc` (properly sized and aligned)
    /// and not accessed through other pointers for `'a`.
    pub unsafe fn new(desc: &'a TypeDesc, ptr: *mut u8) -> Self {
        Slot {
            desc,
            ptr,
            _slot: PhantomData,
        }
    }

    pub fn desc(&self) -> &'a TypeDesc {
        self.desc
    }

    fn at(&self, desc: &'a TypeDesc, offset: usize) -> Slot<'a> {
        // SAFETY: as for `Value::at`; sub-slots of distinct fields/elements are disjoint.
        Slot {
            desc,
            ptr: unsafe { self.ptr.add(offset) },
            _slot: PhantomData,
        }
    }

    /// Store a primitive from its little-endian bytes.
    pub fn write_prim(self, bytes: &[u8]) {
        let n = self.desc.kind().prim_size().expect("not a primitive");
        assert_eq!(bytes.len(), n);
        // SAFETY: the slot has room for the `n`-byte primitive.
        unsafe { slice::from_raw_parts_mut(self.ptr, n) }.copy_from_slice(bytes);
    }

    /// Store a fresh vector of `n` elements; returns slots for them, to be filled in order.
    pub fn init_vector(self, n: usize) -> impl Iterator<Item = Slot<'a>> + 'a {
        let e = self.desc.elem();
        let data = alloc(n * e.size as usize);
        let header = VecHeader {
            data,
            len: n as u64,
            cap: n as u64,
        };
        // SAFETY: a vector slot holds a `VecHeader`.
        unsafe { (self.ptr as *mut MaybeUninit<VecHeader>).as_mut().unwrap() }.write(header);
        let base = Slot {
            desc: e,
            ptr: data,
            _slot: PhantomData,
        };
        (0..n).map(move |i| base.at(e, i * e.size as usize))
    }

    /// Store a fresh `vector<u8>` holding `bytes`.
    pub fn init_bytes(self, bytes: &[u8]) {
        assert!(self.desc.kind() == Kind::Vector && self.desc.elem().kind() == Kind::U8);
        let header = VecHeader::from_bytes(bytes);
        // SAFETY: a vector slot holds a `VecHeader`.
        unsafe { (self.ptr as *mut MaybeUninit<VecHeader>).as_mut().unwrap() }.write(header);
    }

    /// Slots of a struct's fields, in declaration order.
    pub fn fields(self) -> impl Iterator<Item = Slot<'a>> + 'a {
        assert_eq!(self.desc.kind(), Kind::Struct);
        self.desc
            .fields()
            .iter()
            .map(move |f| self.at(f.ty(), f.offset as usize))
    }

    /// Store an enum's tag; returns slots for the variant's fields (`None` if no such tag).
    pub fn init_variant(self, tag: u16) -> Option<impl Iterator<Item = Slot<'a>> + 'a> {
        assert_eq!(self.desc.kind(), Kind::Enum);
        let v = self.desc.variants().get(tag as usize)?;
        // SAFETY: an enum slot starts with its `u16` tag.
        unsafe { (self.ptr as *mut u16).write(tag) };
        let payload = self.at(self.desc, self.desc.payload_offset as usize);
        Some(
            v.fields()
                .iter()
                .map(move |f| payload.at(f.ty(), f.offset as usize)),
        )
    }
}
