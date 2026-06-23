//! Global tokio runtime shared across all FFI calls.
//!
// The runtime is lazily initialized on first use and never shut down.
//! This avoids needing the caller to manage a runtime lifecycle.

use std::sync::OnceLock;
use tokio::runtime::Runtime;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Return a reference to the global tokio runtime.
pub fn global_runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| Runtime::new().expect("failed to create global tokio runtime for FFI"))
}
