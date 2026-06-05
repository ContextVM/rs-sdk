//! Opaque handle type for FFI consumers.

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

/// An opaque handle returned by the FFI layer.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FfiHandle {
    pub id: u64,
}

impl FfiHandle {
    /// Allocate a fresh, unique handle.
    pub fn next() -> Self {
        Self {
            id: NEXT_HANDLE.fetch_add(1, Ordering::Relaxed),
        }
    }
}
