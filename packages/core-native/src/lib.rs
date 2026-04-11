//! diffmind core-native
//!
//! Native Node.js bindings for the shared diffmind engine.

use core_engine::ReviewAnalyzer as InnerAnalyzer;
use napi_derive::napi;

#[napi]
pub struct ReviewAnalyzer {
    inner: InnerAnalyzer,
}

#[napi]
impl ReviewAnalyzer {
    #[napi(constructor)]
    pub fn new(model_bytes: napi::bindgen_prelude::Buffer, tokenizer_bytes: napi::bindgen_prelude::Buffer) -> napi::Result<Self> {
        let inner = InnerAnalyzer::new(&model_bytes, &tokenizer_bytes)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;

        Ok(ReviewAnalyzer { inner })
    }

    /// Analyzes a diff in chunks.
    ///
    /// # Safety
    ///
    /// This method is marked unsafe because it is an async N-API method that takes `&mut self`.
    /// The caller must ensure that this method is not called concurrently on the same instance
    /// from the JavaScript thread, as `napi-rs` cannot statically guarantee mutable access across
    /// the async bridge.
    #[napi]
    pub async unsafe fn analyze_diff_chunked(
        &mut self,
        diff: String,
        context: String,
        max_tokens_per_chunk: u32,
    ) -> napi::Result<String> {
        let findings = self
            .inner
            .analyze_diff_chunked(&diff, &context, max_tokens_per_chunk)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;

        serde_json::to_string(&findings)
            .map_err(|e| napi::Error::from_reason(format!("failed to serialize findings: {e}")))
    }
}
