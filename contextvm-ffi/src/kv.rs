//! Global key-value store that maps FFI handles to their underlying Rust objects.

use parking_lot::Mutex;
use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use crate::handle::FfiHandle;

static STORE: LazyLock<Mutex<HashMap<u64, Arc<dyn Any + Send + Sync>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Insert a value and return its handle.
pub fn insert<T: Send + Sync + 'static>(value: T) -> FfiHandle {
    let handle = FfiHandle::next();
    let mut store = STORE.lock();
    store.insert(handle.id, Arc::new(value));
    handle
}

/// Retrieve a typed handle.
pub fn get<T: Send + Sync + 'static>(handle: FfiHandle) -> Option<Arc<T>> {
    let store = STORE.lock();
    let value = Arc::clone(store.get(&handle.id)?);
    value.downcast::<T>().ok()
}

/// Remove a handle from the store, dropping the object.
pub fn remove(handle: FfiHandle) -> bool {
    STORE.lock().remove(&handle.id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[derive(Debug, Clone)]
    struct TestData {
        value: i32,
    }

    #[test]
    fn test_insert_and_get() {
        let data = TestData { value: 42 };
        let handle = insert(data);
        assert!(handle.id > 0);

        let retrieved = get::<TestData>(handle);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().value, 42);
    }

    #[test]
    fn test_get_invalid_handle() {
        let invalid_handle = FfiHandle { id: 99999 };
        let result = get::<TestData>(invalid_handle);
        assert!(result.is_none());
    }

    #[test]
    fn test_remove() {
        let data = TestData { value: 123 };
        let handle = insert(data);

        assert!(remove(handle));
        assert!(!remove(handle)); // Second remove returns false

        let retrieved = get::<TestData>(handle);
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_concurrent_access() {
        let mut handles = vec![];

        // Spawn multiple threads that insert and read concurrently
        for i in 0..10 {
            let handle = thread::spawn(move || {
                let data = TestData { value: i };
                let h = insert(data);

                // Immediately read back
                let retrieved = get::<TestData>(h);
                assert!(retrieved.is_some());
                assert_eq!(retrieved.unwrap().value, i);

                // Clean up
                remove(h);
            });
            handles.push(handle);
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_concurrent_readers_same_handle() {
        let data = TestData { value: 999 };
        let handle = insert(data);

        let mut threads = vec![];

        for _ in 0..20 {
            let h = handle;
            let t = thread::spawn(move || {
                let retrieved = get::<TestData>(h);
                assert!(retrieved.is_some());
                assert_eq!(retrieved.unwrap().value, 999);
            });
            threads.push(t);
        }

        for t in threads {
            t.join().unwrap();
        }

        remove(handle);
    }

    #[test]
    fn test_arc_semantics() {
        let data = TestData { value: 777 };
        let handle = insert(data);

        // Get multiple Arc references
        let arc1 = get::<TestData>(handle).unwrap();
        let arc2 = get::<TestData>(handle).unwrap();

        // Both should point to same data
        assert_eq!(Arc::strong_count(&arc1), 3); // 1 in store + 2 we got
        assert_eq!(Arc::strong_count(&arc2), 3);

        // Data should be the same
        assert_eq!(arc1.value, 777);
        assert_eq!(arc2.value, 777);

        drop(arc1);
        drop(arc2);

        // Clean up
        remove(handle);
    }

    #[test]
    fn test_different_types() {
        let int_data = 42i32;
        let string_data = "hello".to_string();

        let int_handle = insert(int_data);
        let string_handle = insert(string_data);

        // Should get correct types
        assert_eq!(*get::<i32>(int_handle).unwrap(), 42);
        assert_eq!(*get::<String>(string_handle).unwrap(), "hello");

        // Wrong type should return None
        assert!(get::<String>(int_handle).is_none());
        assert!(get::<i32>(string_handle).is_none());

        remove(int_handle);
        remove(string_handle);
    }
}
