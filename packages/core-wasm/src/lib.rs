//! diffmind core-wasm
//!
//! Wasm bindings for the shared diffmind engine.

use core_engine::ReviewAnalyzer as InnerAnalyzer;
use wasm_bindgen::prelude::*;

// Better panic messages in Node.js / browser console during development
#[cfg(feature = "console_error_panic_hook")]
pub use console_error_panic_hook::set_once as set_panic_hook;

#[wasm_bindgen]
pub struct ReviewAnalyzer {
    inner: InnerAnalyzer,
}

#[wasm_bindgen]
impl ReviewAnalyzer {
    #[wasm_bindgen(constructor)]
    pub fn new(model_bytes: &[u8], tokenizer_bytes: &[u8]) -> Result<ReviewAnalyzer, JsError> {
        #[cfg(feature = "console_error_panic_hook")]
        set_panic_hook();

        let inner = InnerAnalyzer::new(model_bytes, tokenizer_bytes)
            .map_err(|e| JsError::new(&e.to_string()))?;

        Ok(ReviewAnalyzer { inner })
    }

    pub fn analyze_diff_chunked(
        &mut self,
        diff: &str,
        context: &str,
        max_tokens_per_chunk: u32,
    ) -> Result<String, JsError> {
        let findings = self
            .inner
            .analyze_diff_chunked(diff, context, max_tokens_per_chunk)
            .map_err(|e| JsError::new(&e.to_string()))?;

        serde_json::to_string(&findings)
            .map_err(|e| JsError::new(&format!("failed to serialize findings: {e}")))
    }
}
