//! Wall-clock milliseconds, from `Date.now()` in the browser.

#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> f64 {
    #[wasm_bindgen::prelude::wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_namespace = performance, js_name = now)]
        fn performance_now() -> f64;
    }
    performance_now()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> f64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1e3
}
