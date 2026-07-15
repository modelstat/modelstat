//! The native llama.cpp inference backend (feature §10.2, plan D4). Behind the
//! `llama` feature — it links llama.cpp (built via cmake; Metal on macOS, CPU
//! elsewhere), which is why it is optional and quarantined to the engine binary.
//!
//! Implements the [`Backend`] trait the [`crate::Engine`] lifecycle drives: load
//! a GGUF (honoring the GPU-abort guard), and run one blocking completion (the
//! collector's frozen prompt → ChatML → tokens → decode → sample → text). The
//! engine strips `<think>` from the result; the KV-cache reuse across calls
//! (§10.2) is a future optimization — each call opens a fresh context, which is
//! correct, just less efficient.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend as LlamaCppBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

use crate::backend::{Backend, GenParams};

/// llama.cpp must be initialized exactly once per process.
fn cpp_backend() -> &'static LlamaCppBackend {
    static B: OnceLock<LlamaCppBackend> = OnceLock::new();
    B.get_or_init(|| LlamaCppBackend::init().expect("initialize llama.cpp backend"))
}

/// The llama.cpp-backed engine backend.
pub struct LlamaBackend {
    model: Option<LlamaModel>,
    context_size: u32,
    /// Whether we offload to the GPU (Metal). Decided once from platform + guard.
    gpu: bool,
    models_dir: PathBuf,
    version: String,
}

impl LlamaBackend {
    /// `models_dir` holds the `.metal-load-guard`; `version` version-gates it.
    pub fn new(models_dir: impl Into<PathBuf>, version: impl Into<String>) -> Self {
        let models_dir = models_dir.into();
        let version = version.into();
        // Metal only on macOS, and only when the crash guard isn't armed (§10.2).
        let gpu = cfg!(target_os = "macos") && !crate::guard::is_armed(&models_dir, &version);
        Self {
            model: None,
            context_size: 0,
            gpu,
            models_dir,
            version,
        }
    }
}

impl Backend for LlamaBackend {
    fn backend_name(&self) -> &'static str {
        if self.gpu {
            "metal"
        } else {
            "cpu"
        }
    }

    fn load(&mut self, model_path: &Path, context: u32) -> Result<(), String> {
        let n_gpu_layers = if self.gpu { u32::MAX } else { 0 };
        // Arm the guard before the first GPU touch; disarm only after success, so
        // a GPU abort mid-load sticks us to CPU on the next start (§10.2).
        if self.gpu {
            let _ = crate::guard::arm(&self.models_dir, &self.version);
        }
        let params = LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers);
        let model = LlamaModel::load_from_file(cpp_backend(), model_path, &params)
            .map_err(|e| format!("load {}: {e}", model_path.display()))?;
        if self.gpu {
            let _ = crate::guard::disarm(&self.models_dir);
        }
        self.model = Some(model);
        self.context_size = context;
        Ok(())
    }

    fn generate(&mut self, params: &GenParams) -> Result<String, String> {
        let model = self.model.as_ref().ok_or("model not loaded")?;

        // The collector owns the prompt; render it in Qwen's ChatML form.
        let prompt = format!(
            "<|im_start|>system\n{}<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n",
            params.system, params.user
        );

        let mut ctx = model
            .new_context(
                cpp_backend(),
                LlamaContextParams::default().with_n_ctx(NonZeroU32::new(self.context_size)),
            )
            .map_err(|e| format!("new context: {e}"))?;

        let tokens = model
            .str_to_token(&prompt, AddBos::Always)
            .map_err(|e| format!("tokenize: {e}"))?;
        if tokens.is_empty() {
            return Err("empty prompt tokenization".to_string());
        }

        // Prompt pass: only the last token needs logits (we sample from it).
        let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
        let last = tokens.len() - 1;
        for (i, tok) in tokens.iter().enumerate() {
            batch
                .add(*tok, i as i32, &[0], i == last)
                .map_err(|e| format!("batch add: {e}"))?;
        }
        ctx.decode(&mut batch).map_err(|e| format!("decode: {e}"))?;

        let mut sampler = build_sampler(params);
        let mut out = String::new();
        // One decoder for the whole reply so a multi-byte char split across two
        // tokens still decodes correctly.
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let max_ctx = self.context_size as i32;

        for i in 0..params.max_tokens {
            // Sample from the last decoded position's logits.
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(token);
            if model.is_eog_token(token) {
                break;
            }
            if let Ok(piece) = model.token_to_piece(token, &mut decoder, false, None) {
                out.push_str(&piece);
            }

            // The i-th generated token sits just past the prompt.
            let n_cur = tokens.len() as i32 + i as i32;
            if n_cur >= max_ctx {
                break; // don't overflow the context window
            }
            batch.clear();
            batch
                .add(token, n_cur, &[0], true)
                .map_err(|e| format!("batch add: {e}"))?;
            ctx.decode(&mut batch).map_err(|e| format!("decode: {e}"))?;
        }
        Ok(out)
    }

    fn unload(&mut self) {
        self.model = None;
    }
}

/// Sampler chain for the request: greedy at temperature 0, else top-k → temp →
/// distribution with a FIXED seed (deterministic replay, §18).
fn build_sampler(params: &GenParams) -> LlamaSampler {
    if params.temperature <= 0.0 {
        return LlamaSampler::greedy();
    }
    let mut chain = Vec::new();
    if let Some(k) = params.top_k {
        chain.push(LlamaSampler::top_k(k as i32));
    }
    chain.push(LlamaSampler::temp(params.temperature as f32));
    // Fixed seed → deterministic replay so idempotency assertions hold (§18).
    chain.push(LlamaSampler::dist(0x6d6f_6465));
    LlamaSampler::chain_simple(chain)
}
