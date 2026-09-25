//! The engine's JavaScript API (`docs/engine/wasm-api.md`).
//!
//! [`engine::Engine`] is plain Rust and returns `serde_json::Value`s; the
//! `wasm-bindgen` wrapper below exposes it to JS with JSON strings for
//! structured results and typed arrays for row lists. Errors become thrown
//! JS exceptions with a plain-English message.
//!
//! With the `parallel` feature, `initThreadPool(n)` (from
//! `wasm-bindgen-rayon`) must be awaited once before loading a snapshot.

mod clock;
pub mod engine;
pub mod verify;

pub use engine::Engine;

#[cfg(all(feature = "parallel", target_arch = "wasm32"))]
pub use wasm_bindgen_rayon::init_thread_pool;

use wasm_bindgen::prelude::*;

/// JS handle to an [`Engine`].
#[wasm_bindgen(js_name = Engine)]
#[derive(Default, Debug)]
pub struct JsEngine {
    inner: Engine,
}

fn js_err(e: String) -> JsError {
    JsError::new(&e)
}

#[wasm_bindgen(js_class = Engine)]
impl JsEngine {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this build runs on several threads.
    #[wasm_bindgen(js_name = isParallel)]
    pub fn is_parallel() -> bool {
        cfg!(feature = "parallel")
    }

    /// Verifies every hash, then loads the snapshot. Returns a JSON report.
    #[wasm_bindgen(js_name = loadSnapshot)]
    pub fn load_snapshot(
        &mut self,
        manifest: &str,
        data: &[u8],
        cleaning_log: &[u8],
        rejects: &[u8],
    ) -> Result<String, JsError> {
        self.inner
            .load_snapshot(manifest, data, cleaning_log, rejects)
            .map(|v| v.to_string())
            .map_err(js_err)
    }

    /// `{ok, plan_hash, steps: [{op, sentence, node_hash, columns}]}` or
    /// `{ok: false, error, step}`.
    #[wasm_bindgen(js_name = validatePlan)]
    pub fn validate_plan(&self, plan: &str) -> String {
        self.inner.validate_plan(plan).to_string()
    }

    /// Runs a plan: `{ok, execution, plan_hash, ms, steps, output}`.
    pub fn run(&mut self, plan: &str) -> String {
        self.inner.run(plan).to_string()
    }

    /// Output rows `[from, from + count)` as JSON arrays.
    pub fn rows(&self, execution: u32, from: usize, count: usize) -> Result<String, JsError> {
        self.inner
            .rows(execution, from, count)
            .map(|v| v.to_string())
            .map_err(js_err)
    }

    /// Trace summary (row counts per step) for one output row.
    #[wasm_bindgen(js_name = traceBack)]
    pub fn trace_back(&self, execution: u32, row: u32) -> Result<String, JsError> {
        self.inner
            .trace_back(execution, row)
            .map(|(v, _)| v.to_string())
            .map_err(js_err)
    }

    /// The snapshot row indices behind one output row.
    #[wasm_bindgen(js_name = traceBackRows)]
    pub fn trace_back_rows(&self, execution: u32, row: u32) -> Result<Vec<u32>, JsError> {
        self.inner
            .trace_back(execution, row)
            .map(|(_, rows)| rows)
            .map_err(js_err)
    }

    /// Full source records for snapshot rows.
    #[wasm_bindgen(js_name = sourceRecords)]
    pub fn source_records(&self, snapshot: &str, rows: &[u32]) -> Result<String, JsError> {
        self.inner
            .source_records(snapshot, rows)
            .map(|v| v.to_string())
            .map_err(js_err)
    }

    /// Re-derives an execution without the given snapshot rows; returns the
    /// new execution like `run`, plus `method` and `excluded`.
    pub fn exclude(&mut self, execution: u32, rows: &[u32]) -> Result<String, JsError> {
        self.inner
            .exclude(execution, rows)
            .map(|v| v.to_string())
            .map_err(js_err)
    }

    /// Leave-one-out values for one aggregate cell (top `top` movers).
    pub fn contributions(
        &self,
        execution: u32,
        row: u32,
        column: &str,
        top: usize,
    ) -> Result<String, JsError> {
        self.inner
            .contributions(execution, row, column, top)
            .map(|v| v.to_string())
            .map_err(js_err)
    }

    #[wasm_bindgen(js_name = dropExecution)]
    pub fn drop_execution(&mut self, execution: u32) -> bool {
        self.inner.drop_execution(execution)
    }
}
