//! The inference backend abstraction. `Engine` (the lifecycle) is generic over a
//! `Backend`; the real llama.cpp backend, the fail-loud [`UnavailableBackend`],
//! and the test [`MockBackend`] all implement it. The raw model runtime is the
//! ONLY thing a backend owns — download, queueing, idle-unload, the GPU guard,
//! and `<think>`-stripping all live in the `Engine` wrapper.

use std::path::Path;

/// The generation parameters carried over the protocol (`/v1/complete`).
#[derive(Debug, Clone)]
pub struct GenParams {
    pub system: String,
    pub user: String,
    pub temperature: f64,
    pub max_tokens: u32,
    pub top_k: Option<u32>,
}

/// A raw inference backend. Runs on the engine's single serialized worker thread
/// (inference is CPU/GPU-blocking, never on the async runtime).
pub trait Backend: Send + 'static {
    /// `"metal"` | `"cpu"` — reported by `/healthz` (resolvable before load).
    fn backend_name(&self) -> &'static str;
    /// Load the GGUF at `model_path` with the given context window.
    fn load(&mut self, model_path: &Path, context: u32) -> Result<(), String>;
    /// Run inference. Blocking. Returns RAW text — the `Engine` strips `<think>`.
    fn generate(&mut self, params: &GenParams) -> Result<String, String>;
    /// Free the model (idle-unload / shutdown).
    fn unload(&mut self);
}

/// Strip `<think>…</think>` reasoning blocks (feature §10.2) and trim. Unclosed
/// `<think>` drops the remainder (a truncated reasoning tail is never returned).
pub fn strip_think(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end) => rest = &rest[start + end + "</think>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// The fail-loud default when no native inference backend is compiled (the
/// cmake-free build). It NEVER fabricates output — every call errors loudly, so a
/// backend-less engine is honestly broken rather than silently degraded (§21).
pub struct UnavailableBackend;

const NO_BACKEND: &str = "no inference backend is compiled into this engine build \
    — rebuild `modelstat-summarizer` with `--features llama` (requires cmake + a C++ toolchain)";

impl Backend for UnavailableBackend {
    fn backend_name(&self) -> &'static str {
        "cpu"
    }
    fn load(&mut self, _model_path: &Path, _context: u32) -> Result<(), String> {
        Err(NO_BACKEND.to_string())
    }
    fn generate(&mut self, _params: &GenParams) -> Result<String, String> {
        Err(NO_BACKEND.to_string())
    }
    fn unload(&mut self) {}
}

/// A deterministic in-process backend for exercising the engine lifecycle
/// (available to this crate's tests and, via the `mock` feature, to the engine
/// binary's tests). Never linked into a release engine.
#[cfg(any(test, feature = "mock"))]
#[derive(Clone)]
pub struct MockBackend {
    reply: String,
    fail_load: bool,
    fail_generate: bool,
    load_delay: std::time::Duration,
    loaded: bool,
}

#[cfg(any(test, feature = "mock"))]
impl MockBackend {
    /// A healthy backend that returns a fixed summary.
    pub fn ready() -> Self {
        Self {
            reply: "a concise redacted summary".to_string(),
            fail_load: false,
            fail_generate: false,
            load_delay: std::time::Duration::from_millis(0),
            loaded: false,
        }
    }
    pub fn with_reply(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
            ..Self::ready()
        }
    }
    pub fn failing_load() -> Self {
        Self {
            fail_load: true,
            ..Self::ready()
        }
    }
    pub fn failing_generate() -> Self {
        Self {
            fail_generate: true,
            ..Self::ready()
        }
    }
    pub fn with_load_delay(mut self, d: std::time::Duration) -> Self {
        self.load_delay = d;
        self
    }
}

#[cfg(any(test, feature = "mock"))]
impl Backend for MockBackend {
    fn backend_name(&self) -> &'static str {
        "cpu"
    }
    fn load(&mut self, _model_path: &Path, _context: u32) -> Result<(), String> {
        std::thread::sleep(self.load_delay);
        if self.fail_load {
            return Err("mock load failure".to_string());
        }
        self.loaded = true;
        Ok(())
    }
    fn generate(&mut self, params: &GenParams) -> Result<String, String> {
        if !self.loaded {
            return Err("mock backend not loaded".to_string());
        }
        if self.fail_generate {
            return Err("mock inference failure".to_string());
        }
        // Echo a `<think>` block to prove the engine strips it, then the reply.
        Ok(format!(
            "<think>reasoning about {}</think>{}",
            params.user.len(),
            self.reply
        ))
    }
    fn unload(&mut self) {
        self.loaded = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_think_removes_blocks_and_trims() {
        assert_eq!(strip_think("  <think>abc</think>hello  "), "hello");
        assert_eq!(strip_think("a<think>x</think>b<think>y</think>c"), "abc");
        assert_eq!(strip_think("no tags here"), "no tags here");
        // Unclosed → the tail is dropped.
        assert_eq!(strip_think("keep<think>dangling"), "keep");
    }

    #[test]
    fn unavailable_backend_fails_loud() {
        let mut b = UnavailableBackend;
        assert!(b.load(Path::new("/x"), 4096).is_err());
        let p = GenParams {
            system: "s".into(),
            user: "u".into(),
            temperature: 0.2,
            max_tokens: 1024,
            top_k: Some(3),
        };
        assert!(b.generate(&p).unwrap_err().contains("no inference backend"));
    }
}
