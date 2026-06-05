//! Global key-value store that maps FFI handles to their underlying Rust objects.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use crate::handle::FfiHandle;

static STORE: LazyLock<Mutex<HashMap<u64, Arc<dyn Any + Send + Sync>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Insert a value and return its handle.
pub fn insert<T: Send + Sync + 'static>(value: T) -> FfiHandle {
    let handle = FfiHandle::next();
    let mut store = STORE.lock().unwrap();
    store.insert(handle.id, Arc::new(value));
    handle
}

/// Retrieve a typed handle.
pub fn get<T: Send + Sync + 'static>(handle: FfiHandle) -> Option<Arc<T>> {
    let store = STORE.lock().unwrap();
    let value = Arc::clone(store.get(&handle.id)?);
    value.downcast::<T>().ok()
}

/// Remove a handle from the store, dropping the object.
pub fn remove(handle: FfiHandle) -> bool {
    STORE.lock().unwrap().remove(&handle.id).is_some()
}
