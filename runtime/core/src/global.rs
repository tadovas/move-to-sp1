//! Mutable global state for runtime services (e.g. the Sui object store).

use core::cell::UnsafeCell;

/// A global cell. The zkVM is single-threaded, so the only hazard is aliasing: `with` must not
/// be re-entered for the same cell (no `with` call nests inside another on the same global).
pub struct Global<T>(UnsafeCell<T>);

// SAFETY: the zkVM runs a single thread; there is no concurrent access.
unsafe impl<T> Sync for Global<T> {}

impl<T> Global<T> {
    pub const fn new(value: T) -> Self {
        Global(UnsafeCell::new(value))
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        // SAFETY: single-threaded, and callers never nest `with` on the same cell, so this is
        // the only live reference to the contents.
        f(unsafe { &mut *self.0.get() })
    }
}
