# Vendored crates

## wasmparser-0.246.2 (patched)

Source: crates.io `wasmparser 0.246.2`, with ONE change:

```diff
- pub const MAX_WASM_INSTANCES: usize = 1_000;
+ pub const MAX_WASM_INSTANCES: usize = 4_096;
```

Rationale: the component-model validator caps component instances at 1000 as
a DoS guard (not a spec limit). The full wasi-python-layer site (144 native
extensions) + scipy (102) = 246 extensions — over the ~230-extension empirical
cap — so `SandboxFactory` builds fail with "instances count exceeds limit of
1000" at both encode (wit-component) and instantiate (wasmtime). Bumping the
guard to 4096 restores headroom for layer growth.

Both erics validate paths use this exact version: wit-component (aligned to
the 0.246 generation in the workspace) and wasmtime 44. The patch is wired
via `[patch.crates-io]` in the workspace `Cargo.toml`.

To refresh from upstream: re-copy the crate from the cargo registry cache and
re-apply the one-line limits.rs change; keep the README in sync.
