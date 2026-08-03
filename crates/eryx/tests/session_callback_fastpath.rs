//! Integration tests for the session empty-callback fast path state machine.
//!
//! The fast path lets an empty-callback session skip the per-execution callback
//! setup when the pre-initialized empty-callback infrastructure can be reused.
//! It is only safe while the session has never installed a non-empty callback
//! set, because stale callback wrappers could otherwise survive into later
//! (empty) executions. A full `reset()` re-instantiates and restores
//! eligibility (C4 + A3 coverage).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use eryx::callback_handler::run_callback_handler;
use eryx::secrets::SecretConfig;
use eryx::{Callback, CallbackError, PythonExecutor, ResourceLimits, SessionExecutor, TypedCallback};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Shared executor to avoid repeated WASM loading across tests.
static SHARED_EXECUTOR: LazyLock<Arc<PythonExecutor>> = LazyLock::new(|| Arc::new(create_executor()));

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

// =============================================================================
// Test Callback
// =============================================================================

/// A callback that always succeeds and returns a simple response.
struct SucceedCallback;

impl TypedCallback for SucceedCallback {
    type Args = ();

    fn name(&self) -> &str {
        "succeed"
    }

    fn description(&self) -> &str {
        "Always succeeds with a simple response"
    }

    fn invoke_typed(
        &self,
        _args: (),
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"status": "ok"})) })
    }
}

/// Run a session execution with the `succeed` callback wired up.
async fn run_with_callback(session: &mut SessionExecutor, code: &str) -> eryx::ExecutionOutput {
    let cb: Arc<dyn Callback> = Arc::new(SucceedCallback);
    let callbacks_map: Arc<HashMap<String, Arc<dyn Callback>>> =
        Arc::new(HashMap::from([("succeed".to_string(), cb.clone())]));
    let callbacks_vec: Vec<Arc<dyn Callback>> = callbacks_map.values().cloned().collect();

    let (callback_tx, callback_rx) = mpsc::channel(8);
    let handler = tokio::spawn(run_callback_handler(
        callback_rx,
        callbacks_map,
        ResourceLimits::default(),
        Arc::<HashMap<String, SecretConfig>>::new(HashMap::new()),
    ));

    let result = session
        .execute(code)
        .with_callbacks(&callbacks_vec, callback_tx)
        .run()
        .await
        .expect("Execution with callbacks should succeed");
    handler.await.expect("Callback handler should finish");
    result
}

// =============================================================================
// Fast Path State Machine Tests
// =============================================================================

#[tokio::test]
async fn test_empty_callback_execution_takes_fast_path() {
    // Session A: constructed empty -> the fast path stays eligible forever.
    let mut a = SessionExecutor::new(get_shared_executor(), &[])
        .await
        .expect("Failed to create session A");
    let a_out = a
        .execute("x = 1")
        .run()
        .await
        .expect("Empty-callback execution should succeed");
    let fuel_a = a_out.fuel_consumed.expect("fuel should be tracked");

    // Session B: constructed with a callback -> the fast path is disabled for
    // the lifetime of the instance, so its EMPTY execution still performs the
    // full callback setup and consumes visibly more fuel.
    let cb: Arc<dyn Callback> = Arc::new(SucceedCallback);
    let mut b = SessionExecutor::new(get_shared_executor(), &[cb])
        .await
        .expect("Failed to create session B");
    run_with_callback(&mut b, "x = 1").await;
    let b_out = b
        .execute("x = 1")
        .run()
        .await
        .expect("Empty-callback execution should succeed");
    let fuel_b = b_out.fuel_consumed.expect("fuel should be tracked");

    assert!(
        fuel_b > fuel_a,
        "full callback setup must consume more fuel than the fast path \
         (fast={fuel_a}, full={fuel_b})"
    );
    eprintln!("fast-path fuel: {fuel_a}, full-setup fuel: {fuel_b}");
}

#[tokio::test]
async fn test_callbacks_work_when_installed() {
    // Empty-constructed session; a non-empty execution must install callbacks
    // (the fast path must not interfere with callback installation).
    let mut session = SessionExecutor::new(get_shared_executor(), &[])
        .await
        .expect("Failed to create session");
    let output = run_with_callback(&mut session, "result = await succeed()").await;
    let result = output.result.expect("result should be captured");
    assert!(
        result.contains("status") && result.contains("ok"),
        "callback result missing payload: {result}"
    );
}

#[tokio::test]
async fn test_empty_execution_after_non_empty_has_no_callback_surface() {
    // After a non-empty execution, an empty execution must not expose the
    // previous callback set: introspection shows an empty list and invoking a
    // stale name fails cleanly instead of reaching the host.
    let cb: Arc<dyn Callback> = Arc::new(SucceedCallback);
    let mut session = SessionExecutor::new(get_shared_executor(), &[cb])
        .await
        .expect("Failed to create session");
    run_with_callback(&mut session, "x = 1").await;

    // Introspection: the empty execution sees no callbacks.
    let introspect = session
        .execute("print(list_callbacks())")
        .run()
        .await
        .expect("Introspection should succeed");
    assert_eq!(
        introspect.stdout.trim(),
        "[]",
        "stale callback wrappers leaked into the empty execution: {:?}",
        introspect.stdout
    );

    // Invocation: a stale name must not reach a host handler.
    let probe = session
        .execute(
            "try:\n    await succeed()\n    print('leaked')\nexcept Exception:\n    print('blocked')",
        )
        .run()
        .await
        .expect("Probe should complete");
    assert_eq!(
        probe.stdout.trim(),
        "blocked",
        "stale callback invocation leaked through: {:?}",
        probe.stdout
    );
}

#[tokio::test]
async fn test_reset_restores_fast_path() {
    // Session B disabled the fast path by installing callbacks; after reset()
    // (fresh instance) an empty execution must be fast again.
    let cb: Arc<dyn Callback> = Arc::new(SucceedCallback);
    let mut b = SessionExecutor::new(get_shared_executor(), &[cb])
        .await
        .expect("Failed to create session B");
    run_with_callback(&mut b, "x = 1").await;
    let full_out = b
        .execute("x = 1")
        .run()
        .await
        .expect("Empty execution should succeed");
    let fuel_full = full_out.fuel_consumed.expect("fuel should be tracked");

    b.reset(&[]).await.expect("Reset should succeed");
    let reset_out = b
        .execute("x = 1")
        .run()
        .await
        .expect("Empty execution after reset should succeed");
    let fuel_reset = reset_out.fuel_consumed.expect("fuel should be tracked");

    assert!(
        fuel_reset < fuel_full,
        "reset() must restore the empty-callback fast path \
         (full={fuel_full}, after-reset={fuel_reset})"
    );
    eprintln!("full-setup fuel: {fuel_full}, after-reset fuel: {fuel_reset}");
}
