//! Integration tests for session memory-limit enforcement.
//!
//! Verifies that a configured memory limit rejects WASM memory growth beyond
//! the limit (C6), and that the limit survives a full `reset()` which
//! re-instantiates the component.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, LazyLock};

use eryx::{Error, PythonExecutor, SessionExecutor};

/// Shared executor to avoid repeated WASM loading across tests.
static SHARED_EXECUTOR: LazyLock<Arc<PythonExecutor>> =
    LazyLock::new(|| Arc::new(create_executor()));

fn get_shared_executor() -> Arc<PythonExecutor> {
    SHARED_EXECUTOR.clone()
}

/// Create a PythonExecutor, using embedded resources if available.
fn create_executor() -> PythonExecutor {
    #[cfg(feature = "embedded")]
    {
        let resources =
            eryx::embedded::EmbeddedResources::get().expect("Failed to extract embedded resources");

        #[allow(unsafe_code)]
        unsafe { PythonExecutor::from_precompiled_file(resources.runtime()) }
            .expect("Failed to load embedded runtime")
            .with_python_stdlib(resources.stdlib())
    }

    #[cfg(not(feature = "embedded"))]
    {
        let stdlib_path = python_stdlib_path();
        let path = runtime_wasm_path();
        PythonExecutor::from_file(&path)
            .unwrap_or_else(|e| panic!("Failed to load runtime.wasm from {:?}: {}", path, e))
            .with_python_stdlib(&stdlib_path)
    }
}

#[cfg(not(feature = "embedded"))]
fn runtime_wasm_path() -> std::path::PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-runtime")
        .join("runtime.wasm")
}

#[cfg(not(feature = "embedded"))]
fn python_stdlib_path() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("ERYX_PYTHON_STDLIB") {
        let path = std::path::PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-wasm-runtime")
        .join("tests")
        .join("python-stdlib")
}

/// Create a session with the given WASM memory limit.
async fn create_limited_session(memory_limit: Option<u64>) -> SessionExecutor {
    let executor = get_shared_executor();
    SessionExecutor::new_with_limits(executor, &[], memory_limit)
        .await
        .expect("Failed to create session")
}

/// Code that allocates 64 MiB of guest linear memory.
const ALLOC_CODE: &str = "x = bytearray(64 * 1024 * 1024)";

#[tokio::test]
async fn test_memory_limit_rejects_growth_beyond_limit() {
    // Measure the peak memory of an unlimited session running the allocation.
    let mut unlimited = create_limited_session(None).await;
    let output = unlimited
        .execute(ALLOC_CODE)
        .run()
        .await
        .expect("Unlimited session should allocate 64 MiB");
    let peak = output.peak_memory_bytes;
    assert!(
        peak > 64 * 1024 * 1024,
        "peak ({peak} bytes) should exceed the 64 MiB allocation"
    );

    // A session limited just below that peak must reject the same allocation.
    let mut limited = create_limited_session(Some(peak - 1)).await;
    let result = limited.execute(ALLOC_CODE).run().await;
    assert!(
        result.is_err(),
        "Allocation beyond the memory limit must fail, got: {result:?}"
    );
}

#[tokio::test]
async fn test_memory_limit_above_peak_permits_allocation() {
    let mut unlimited = create_limited_session(None).await;
    let output = unlimited
        .execute(ALLOC_CODE)
        .run()
        .await
        .expect("Unlimited session should allocate 64 MiB");
    let peak = output.peak_memory_bytes;

    let mut roomy = create_limited_session(Some(peak + (1 << 20))).await;
    roomy
        .execute(ALLOC_CODE)
        .run()
        .await
        .expect("Limit above the peak must permit the allocation");
}

#[tokio::test]
async fn test_memory_limit_preserved_across_reset() {
    let mut unlimited = create_limited_session(None).await;
    let output = unlimited
        .execute(ALLOC_CODE)
        .run()
        .await
        .expect("Unlimited session should allocate 64 MiB");
    let peak = output.peak_memory_bytes;

    // Fresh limited session: baseline execution works, big allocation fails.
    let mut session = create_limited_session(Some(peak - 1)).await;
    session
        .execute("x = 1")
        .run()
        .await
        .expect("Baseline execution should succeed under the limit");
    assert!(
        session.execute(ALLOC_CODE).run().await.is_err(),
        "Allocation beyond the limit must fail before reset"
    );

    // reset() re-instantiates the component; the limit must be preserved.
    session.reset(&[]).await.expect("Reset should succeed");
    assert!(
        session.execute(ALLOC_CODE).run().await.is_err(),
        "The memory limit must be enforced after reset()"
    );
    session
        .execute("x = 1")
        .run()
        .await
        .expect("Session must remain usable after reset");
}

#[tokio::test]
async fn test_memory_limit_error_is_reported() {
    // The failure must surface as a typed Error, not a panic or hang.
    let mut unlimited = create_limited_session(None).await;
    let output = unlimited
        .execute(ALLOC_CODE)
        .run()
        .await
        .expect("Unlimited session should allocate 64 MiB");
    let peak = output.peak_memory_bytes;

    let mut limited = create_limited_session(Some(peak - 1)).await;
    let result = limited.execute(ALLOC_CODE).run().await;
    match &result {
        Err(Error::Execution(_)) | Err(Error::PythonException(_)) | Err(Error::WasmEngine(_)) => {}
        other => panic!("expected a typed execution error, got: {other:?}"),
    }
}
