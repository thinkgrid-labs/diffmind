//! Local GGUF inference via candle, with constrained JSON decoding.

use crate::backend::{GenOptions, ReviewBackend};
use crate::error::EngineError;
use crate::json_guard::JsonPrefix;
use crate::prompt::Prompt;
use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::models::quantized_qwen2::ModelWeights as Qwen2;
use std::path::Path;

/// Which compute device to use for inference.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DevicePreference {
    /// Metal on Apple Silicon, CPU everywhere else.
    #[default]
    Auto,
    /// Force CPU inference on all platforms.
    Cpu,
    /// Force Metal. Errors on any build that cannot use it, rather than
    /// failing later during inference.
    Metal,
}

impl DevicePreference {
    pub fn parse(s: &str) -> DevicePreference {
        match s.trim().to_lowercase().as_str() {
            "metal" | "gpu" => DevicePreference::Metal,
            "cpu" => DevicePreference::Cpu,
            _ => DevicePreference::Auto,
        }
    }
}

/// Whether this build can safely run candle's Metal backend.
///
/// `Device::new_metal(0)` succeeding is NOT sufficient. An Intel Mac reports
/// Metal 3 support and opens the device fine, but candle's matmul kernels are
/// written against Apple-GPU-only SIMD-group matrix intrinsics. Probed on an
/// Intel Iris Plus 655 (macOS 15.7.7):
///
/// ```text
/// Device::new_metal(0) -> OK
///   OK    f32 alloc
///   OK    f32 add
///   PANIC f32 matmul
/// ```
///
/// Allocation and elementwise ops work; the first `matmul` — which every
/// forward pass needs — dies building its compute pipeline:
///
/// ```text
/// thread 'main' panicked at candle-metal-kernels-0.10.2/src/metal/device.rs:111
///   NSError { code: 2, "AIR builtin function was called but no definition was
///             found.", domain: "CompilerError" }
/// ```
///
/// That is an `unwrap()` inside candle, and the release profile sets
/// `panic = "abort"`, so it cannot be caught and turned into a CPU fallback
/// at runtime. The only reliable fix is to not select Metal on hardware whose
/// kernels do not exist. Gating on the target architecture does that: the
/// release matrix ships a native `aarch64-apple-darwin` build for Apple
/// Silicon (which keeps Metal) and `x86_64-apple-darwin` for Intel Macs
/// (which get CPU + Accelerate BLAS).
pub const METAL_SUPPORTED: bool = cfg!(all(target_os = "macos", target_arch = "aarch64"));

/// Open a Metal device, or explain precisely why this build cannot.
fn metal_device() -> Result<Device, EngineError> {
    if !METAL_SUPPORTED {
        let reason = if cfg!(target_os = "macos") {
            "Metal inference requires an Apple Silicon GPU. This is the Intel build, \
             and candle's matmul kernels need Apple-GPU-only intrinsics. Use --device cpu."
        } else {
            "Metal is only available on macOS. Use --device cpu."
        };
        return Err(EngineError::DeviceUnavailable(reason.into()));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Device::new_metal(0)
            .map_err(|e| EngineError::DeviceUnavailable(format!("Metal unavailable: {e}")));
    }

    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    unreachable!("guarded by METAL_SUPPORTED")
}

/// Select the best available device according to the caller's preference.
/// Returns the device and a one-line label for the CLI header.
pub fn resolve_device(pref: &DevicePreference) -> Result<(Device, String), EngineError> {
    match pref {
        DevicePreference::Cpu => Ok((Device::Cpu, "CPU".into())),

        DevicePreference::Metal => metal_device().map(|d| (d, "Metal (Apple Silicon GPU)".into())),

        DevicePreference::Auto => {
            if METAL_SUPPORTED {
                match metal_device() {
                    Ok(d) => return Ok((d, "Metal (Apple Silicon GPU)".into())),
                    Err(_) => {
                        return Ok((
                            Device::Cpu,
                            "CPU (Metal unavailable, using Accelerate BLAS)".into(),
                        ));
                    }
                }
            }
            if cfg!(target_os = "macos") {
                Ok((
                    Device::Cpu,
                    "CPU (Intel Mac — Metal needs Apple Silicon, using Accelerate BLAS)".into(),
                ))
            } else {
                Ok((Device::Cpu, "CPU".into()))
            }
        }
    }
}

/// Fallback context window when the GGUF file does not declare one.
const FALLBACK_CONTEXT_TOKENS: usize = 4096;

/// Upper bound regardless of what the file claims. A 32K KV cache at 32B is
/// enough to swap a laptop to death, and no review prompt needs it.
const MAX_USABLE_CONTEXT_TOKENS: usize = 32_768;

pub struct CandleBackend {
    device: Device,
    device_label: String,
    model: Qwen2,
    tokenizer: tokenizers::Tokenizer,
    context_tokens: usize,
    model_label: String,
    eos_ids: Vec<u32>,
}

impl CandleBackend {
    /// Load from a memory-mapped GGUF file.
    ///
    /// Reading the file into a `Vec` first (as the CLI used to) doubles peak
    /// RSS: the whole quantized model sits on the heap while candle copies
    /// tensors out of it. At 7B that is the difference between fitting in the
    /// advertised 8 GB and swapping.
    pub fn from_path(
        model_path: &Path,
        tokenizer_path: &Path,
        pref: DevicePreference,
    ) -> Result<Self, EngineError> {
        let file = std::fs::File::open(model_path)
            .map_err(|e| EngineError::ModelLoadError(format!("{}: {e}", model_path.display())))?;
        // SAFETY: the model file is owned by diffmind under ~/.diffmind/models
        // and is only rewritten by an atomic rename, never modified in place,
        // so the mapping cannot be mutated underneath us mid-read.
        let mmap = unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| EngineError::ModelLoadError(format!("mmap failed: {e}")))?;

        let tokenizer_bytes = std::fs::read(tokenizer_path).map_err(|e| {
            EngineError::TokenizerError(format!("{}: {e}", tokenizer_path.display()))
        })?;

        let label = model_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("model")
            .to_string();

        Self::from_bytes(&mmap, &tokenizer_bytes, pref, label)
    }

    pub fn from_bytes(
        model_bytes: &[u8],
        tokenizer_bytes: &[u8],
        pref: DevicePreference,
        model_label: String,
    ) -> Result<Self, EngineError> {
        let (device, device_label) = resolve_device(&pref)?;

        let tokenizer = tokenizers::Tokenizer::from_bytes(tokenizer_bytes)
            .map_err(|e| EngineError::TokenizerError(e.to_string()))?;

        let mut reader = std::io::Cursor::new(model_bytes);
        let gguf = gguf_file::Content::read(&mut reader)
            .map_err(|e| EngineError::GgufError(e.to_string()))?;

        let context_tokens = context_from_metadata(&gguf).unwrap_or(FALLBACK_CONTEXT_TOKENS);
        let context_tokens = context_tokens.clamp(1024, MAX_USABLE_CONTEXT_TOKENS);

        let model = Qwen2::from_gguf(gguf, &mut reader, &device)
            .map_err(|e| EngineError::ModelLoadError(e.to_string()))?;

        let eos_ids = ["<|im_end|>", "<|endoftext|>"]
            .iter()
            .filter_map(|t| tokenizer.token_to_id(t))
            .collect();

        Ok(CandleBackend {
            device,
            device_label,
            model,
            tokenizer,
            context_tokens,
            model_label,
            eos_ids,
        })
    }

    pub fn device_label(&self) -> &str {
        &self.device_label
    }

    /// Decode a single token to the text it contributes.
    /// `None` when the token is half of a multi-byte character and cannot be
    /// judged on its own.
    fn decode_piece(&self, id: u32) -> Option<String> {
        self.tokenizer
            .decode(&[id], true)
            .ok()
            .filter(|s| !s.contains('\u{FFFD}'))
    }
}

/// Pull the context window out of GGUF metadata.
///
/// The key is namespaced by architecture (`qwen2.context_length`), so match on
/// the suffix rather than hardcoding a family name — this keeps working if the
/// model catalog ever gains a non-Qwen entry. The value was previously
/// hardcoded to 4096, which is a quarter of what Qwen2.5-Coder actually
/// supports and forced needless chunking.
fn context_from_metadata(gguf: &gguf_file::Content) -> Option<usize> {
    gguf.metadata
        .iter()
        .find(|(k, _)| k.ends_with(".context_length"))
        .and_then(|(_, v)| {
            v.to_u32()
                .map(|n| n as usize)
                .or_else(|_| v.to_u64().map(|n| n as usize))
                .ok()
        })
        .filter(|n| *n > 0)
}

/// Deterministic, dependency-free PRNG (SplitMix64) so sampling reproducibility
/// does not hinge on a `rand` minor version.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // 24 bits of mantissa is ample for choosing among a few dozen tokens.
        ((z >> 40) as f32) / ((1u32 << 24) as f32)
    }
}

/// How many of the highest-scoring tokens to consider when the JSON constraint
/// rejects the top pick. Deep enough to always find a structurally valid
/// continuation in practice, shallow enough to cost nothing next to a forward pass.
const CANDIDATE_POOL: usize = 64;

impl ReviewBackend for CandleBackend {
    fn describe(&self) -> String {
        format!("{} · {}", self.model_label, self.device_label)
    }

    fn context_tokens(&self) -> usize {
        self.context_tokens
    }

    fn count_tokens(&self, text: &str) -> Option<usize> {
        self.tokenizer.encode(text, true).ok().map(|t| t.len())
    }

    fn supports_constrained_json(&self) -> bool {
        true
    }

    fn generate(&mut self, prompt: &Prompt, opts: &GenOptions) -> Result<String, EngineError> {
        let rendered = prompt.to_chatml();

        let encoding = self
            .tokenizer
            .encode(rendered.as_str(), true)
            .map_err(|e| EngineError::TokenizerError(e.to_string()))?;
        let mut token_ids = encoding.get_ids().to_vec();
        let prompt_len = token_ids.len();

        if prompt_len >= self.context_tokens {
            return Err(EngineError::ForwardError(format!(
                "prompt is too long ({} tokens, limit {}); reduce the diff or context",
                prompt_len, self.context_tokens
            )));
        }

        let max_new = opts.max_new_tokens.min(self.context_tokens - prompt_len);

        if opts.debug {
            eprintln!("[debug] prompt tokens: {prompt_len}  max_new: {max_new}");
        }

        let mut rng = SplitMix64(opts.seed);
        let mut guard = opts.json.then(JsonPrefix::new);
        // Set if the guard ever desynchronises from the emitted text; we fail
        // open rather than constrain against a state we no longer trust.
        let mut guard_disabled = false;
        let mut text = String::new();
        // Length of `text` at the most recent point the JSON could have been
        // closed cleanly. Running out of tokens mid-element rewinds here.
        let mut last_closable_len: Option<usize> = None;

        for step in 0..max_new {
            let context_size = if step > 0 { 1 } else { token_ids.len() };
            let start_pos = token_ids.len() - context_size;

            let input = Tensor::new(&token_ids[start_pos..], &self.device)?.unsqueeze(0)?;

            let logits = self
                .model
                .forward(&input, start_pos)
                .map_err(|e| EngineError::ForwardError(e.to_string()))?;

            if logits.elem_count() == 0 {
                break;
            }

            let logits = logits.squeeze(0)?;
            // The model returns [batch, vocab] or [batch, seq, vocab]; after
            // squeeze we want the final position's row.
            let logits = match logits.dims().len() {
                1 => logits,
                2 => {
                    let rows = logits.dim(0)?;
                    if rows == 0 {
                        break;
                    }
                    logits.get(rows - 1)?
                }
                _ => {
                    return Err(EngineError::ForwardError(format!(
                        "unexpected logits shape: {:?}",
                        logits.dims()
                    )));
                }
            };

            // Discourage the degenerate loops small models fall into. Without
            // this, a 0.5B routinely repeats the same finding until the cap.
            let logits = if opts.repeat_penalty > 1.0 && !token_ids.is_empty() {
                let from = token_ids.len().saturating_sub(opts.repeat_last_n);
                candle_transformers::utils::apply_repeat_penalty(
                    &logits,
                    opts.repeat_penalty,
                    &token_ids[from..],
                )
                .map_err(|e| EngineError::SamplingError(e.to_string()))?
            } else {
                logits
            };

            let scores = logits
                .to_dtype(candle_core::DType::F32)?
                .to_vec1::<f32>()
                .map_err(|e| EngineError::SamplingError(e.to_string()))?;

            let active_guard = if guard_disabled { None } else { guard.as_ref() };
            let Some(next) = self.choose_token(&scores, active_guard, opts, &mut rng) else {
                // Nothing in the pool keeps the JSON valid. Stop and let the
                // repair pass close whatever is open.
                if opts.debug {
                    eprintln!("[debug] no structurally valid candidate at step {step}; stopping");
                }
                break;
            };

            if self.eos_ids.contains(&next) {
                break;
            }

            token_ids.push(next);

            if let Some(piece) = self.decode_piece(next) {
                if let Some(g) = guard.as_mut()
                    && !guard_disabled
                    && g.push_str(&piece).is_err()
                {
                    // Only reachable when single-token decoding disagrees with
                    // what the constraint check saw (multi-byte boundaries).
                    guard_disabled = true;
                    if opts.debug {
                        eprintln!("[debug] JSON guard desynchronised; continuing unconstrained");
                    }
                }
                text.push_str(&piece);
            } else {
                // Undecodable in isolation — recover the exact contribution by
                // decoding the whole completion and taking what is new.
                let full = self
                    .tokenizer
                    .decode(&token_ids[prompt_len..], true)
                    .map_err(|e| EngineError::TokenizerError(e.to_string()))?;
                if let Some(piece) = full.strip_prefix(text.as_str()) {
                    let piece = piece.to_string();
                    if let Some(g) = guard.as_mut()
                        && !guard_disabled
                        && g.push_str(&piece).is_err()
                    {
                        guard_disabled = true;
                    }
                    text.push_str(&piece);
                } else {
                    text = full;
                    guard_disabled = true;
                }
            }

            if let Some(g) = guard.as_ref()
                && !guard_disabled
            {
                // Stop the instant the answer is syntactically closed;
                // everything after it is commentary that gets discarded anyway.
                if g.is_complete() {
                    break;
                }
                if g.is_closable() {
                    last_closable_len = Some(text.len());
                }
            }
        }

        // Salvage a run that hit the cap mid-object. The findings already
        // emitted are usually the ones worth having, and throwing the whole
        // chunk away is how "N chunks returned unusable output" happened.
        if let Some(g) = guard.as_ref()
            && !guard_disabled
            && !g.is_complete()
        {
            match crate::json_guard::repair_truncated(&text, last_closable_len) {
                Some(repaired) => {
                    if opts.debug {
                        eprintln!(
                            "[debug] repaired truncated JSON ({} -> {} bytes)",
                            text.len(),
                            repaired.len()
                        );
                    }
                    text = repaired;
                }
                None if opts.debug => {
                    eprintln!("[debug] output was truncated beyond repair");
                }
                None => {}
            }
        }

        if opts.debug {
            eprintln!("[debug] raw model output:\n{text}\n");
        }

        Ok(text)
    }
}

impl CandleBackend {
    /// Pick the next token: the highest-scoring candidate that keeps the output
    /// a valid JSON prefix (or, above zero temperature, a sample from those).
    fn choose_token(
        &self,
        scores: &[f32],
        guard: Option<&JsonPrefix>,
        opts: &GenOptions,
        rng: &mut SplitMix64,
    ) -> Option<u32> {
        let permitted = |id: u32| -> bool {
            let Some(g) = guard else { return true };
            if self.eos_ids.contains(&id) {
                // Ending is only allowed once the document actually closes.
                return g.is_complete();
            }
            match self.decode_piece(id) {
                Some(piece) => g.accepts(&piece),
                // Undecodable alone: cannot be judged, so allow it and let the
                // post-commit check catch any desync.
                None => true,
            }
        };

        // Fast path: greedy decoding almost always accepts the single best
        // token, so find it with one pass and no allocation. Only when the JSON
        // constraint rejects it do we pay for ranking a candidate pool.
        if opts.temperature <= 0.0
            && let Some(best) = argmax(scores)
            && permitted(best)
        {
            return Some(best);
        }

        let ranked = top_k(scores, CANDIDATE_POOL);

        if opts.temperature <= 0.0 {
            return ranked
                .iter()
                .find(|(id, _)| permitted(*id))
                .map(|(id, _)| *id);
        }

        let valid: Vec<(u32, f32)> = ranked
            .into_iter()
            .filter(|(id, _)| permitted(*id))
            .collect();
        if valid.is_empty() {
            return None;
        }

        // Softmax over the permitted candidates only.
        let max = valid[0].1;
        let temp = opts.temperature as f32;
        let weights: Vec<f32> = valid
            .iter()
            .map(|(_, s)| ((s - max) / temp).exp())
            .collect();
        let total: f32 = weights.iter().sum();
        if !total.is_finite() || total <= 0.0 {
            return Some(valid[0].0);
        }

        let target = rng.next_f32() * total;
        let mut acc = 0.0;
        for ((id, _), w) in valid.iter().zip(&weights) {
            acc += w;
            if acc >= target {
                return Some(*id);
            }
        }
        valid.last().map(|(id, _)| *id)
    }
}

/// Index of the highest score, without allocating.
fn argmax(scores: &[f32]) -> Option<u32> {
    let mut best: Option<(usize, f32)> = None;
    for (i, &s) in scores.iter().enumerate() {
        if !s.is_finite() {
            continue;
        }
        if best.is_none_or(|(_, b)| s > b) {
            best = Some((i, s));
        }
    }
    best.map(|(i, _)| i as u32)
}

/// The `k` highest-scoring tokens, descending.
///
/// A vocabulary is ~150k entries and this runs once per generated token, so the
/// obvious `collect().sort()` would allocate and sort 150k tuples per token.
/// This keeps a bounded buffer and touches the heap once.
fn top_k(scores: &[f32], k: usize) -> Vec<(u32, f32)> {
    let k = k.max(1);
    let mut best: Vec<(u32, f32)> = Vec::with_capacity(k + 1);
    // Score to beat before an entry is worth considering at all.
    let mut cutoff = f32::NEG_INFINITY;

    for (i, &s) in scores.iter().enumerate() {
        if !s.is_finite() || (best.len() == k && s <= cutoff) {
            continue;
        }
        let pos = best
            .iter()
            .position(|(_, existing)| s > *existing)
            .unwrap_or(best.len());
        best.insert(pos, (i as u32, s));
        if best.len() > k {
            best.pop();
        }
        if best.len() == k {
            cutoff = best[k - 1].1;
        }
    }

    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--device auto` must never hand back a Metal device on a build that
    /// cannot run candle's matmul kernels — doing so aborts mid-inference
    /// instead of falling back. See `METAL_SUPPORTED`.
    #[test]
    fn auto_device_is_cpu_unless_metal_is_supported() {
        let (dev, _) = resolve_device(&DevicePreference::Auto).expect("auto must always resolve");
        if METAL_SUPPORTED {
            assert!(dev.is_metal() || dev.is_cpu());
        } else {
            assert!(
                dev.is_cpu(),
                "auto selected a non-CPU device on a build without Metal support"
            );
        }
    }

    /// Explicit `--device metal` on an unsupported build must return a clean
    /// error the CLI can print, not panic deep inside candle.
    #[test]
    fn explicit_metal_errors_cleanly_when_unsupported() {
        if METAL_SUPPORTED {
            return;
        }
        let err = resolve_device(&DevicePreference::Metal)
            .expect_err("metal must be refused when unsupported");
        assert!(err.to_string().contains("--device cpu"));
    }

    #[test]
    fn device_preference_parsing() {
        assert_eq!(DevicePreference::parse("METAL"), DevicePreference::Metal);
        assert_eq!(DevicePreference::parse("cpu"), DevicePreference::Cpu);
        assert_eq!(DevicePreference::parse("anything"), DevicePreference::Auto);
    }

    #[test]
    fn top_k_returns_the_highest_scores_in_order() {
        let scores = [0.1, 5.0, 2.0, 9.0, 3.0, -1.0];
        let top = top_k(&scores, 3);
        assert_eq!(top, vec![(3, 9.0), (1, 5.0), (4, 3.0)]);
    }

    #[test]
    fn top_k_handles_a_pool_larger_than_the_vocabulary() {
        assert_eq!(top_k(&[1.0, 2.0], 64).len(), 2);
        assert!(top_k(&[], 8).is_empty());
    }

    #[test]
    fn top_k_and_argmax_skip_non_finite_scores() {
        // A masked logit arrives as -inf, and NaN would poison every comparison.
        let scores = [f32::NAN, 1.0, f32::NEG_INFINITY, 4.0];
        assert_eq!(argmax(&scores), Some(3));
        assert_eq!(top_k(&scores, 2), vec![(3, 4.0), (1, 1.0)]);
    }

    #[test]
    fn argmax_agrees_with_the_head_of_top_k() {
        let scores: Vec<f32> = (0..500).map(|i| ((i * 37) % 101) as f32).collect();
        assert_eq!(argmax(&scores), Some(top_k(&scores, 5)[0].0));
    }

    #[test]
    fn splitmix_is_deterministic_and_in_range() {
        let mut a = SplitMix64(42);
        let mut b = SplitMix64(42);
        for _ in 0..100 {
            let x = a.next_f32();
            assert_eq!(x, b.next_f32(), "same seed must replay identically");
            assert!((0.0..=1.0).contains(&x), "out of range: {x}");
        }
        let mut c = SplitMix64(43);
        assert_ne!(SplitMix64(42).next_f32(), c.next_f32());
    }
}
