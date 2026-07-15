//! Engine config — `summarizer.json` (feature §10.3) + the pinned model defaults
//! and dev/test env overrides (§10.2/§19).

use std::path::{Path, PathBuf};

use modelstat_download::DownloadSpec;
use serde::{Deserialize, Serialize};

pub const DEFAULT_BIND: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 4321;
pub const DEFAULT_CONTEXT: u32 = 4096;
pub const DEFAULT_PARALLEL: u32 = 1;
/// 15 minutes; `0` = keep resident (§10.2).
pub const DEFAULT_IDLE_UNLOAD_MS: u64 = 15 * 60 * 1000;

/// The pinned Qwen build (§10.2). sha256 is pinned per release — left None until
/// the release pipeline wires it; the downloader simply skips verification then.
pub const DEFAULT_MODEL_URL: &str =
    "https://huggingface.co/lmstudio-community/Qwen3.5-4B-GGUF/resolve/main/Qwen3.5-4B-Q4_K_M.gguf";
pub const MODEL_FILE_NAME: &str = "Qwen3.5-4B-Q4_K_M.gguf";
pub const MODEL_SIZE_LABEL: &str = "~2.7 GB";

fn d_bind() -> String {
    DEFAULT_BIND.to_string()
}
fn d_port() -> u16 {
    DEFAULT_PORT
}
fn d_context() -> u32 {
    DEFAULT_CONTEXT
}
fn d_parallel() -> u32 {
    DEFAULT_PARALLEL
}
fn d_idle() -> u64 {
    DEFAULT_IDLE_UNLOAD_MS
}

/// `~/.modelstat/summarizer.json` (§10.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default = "d_bind")]
    pub bind: String,
    #[serde(default = "d_port")]
    pub port: u16,
    pub model_path: PathBuf,
    #[serde(default = "d_context")]
    pub context: u32,
    #[serde(default = "d_parallel")]
    pub parallel: u32,
    #[serde(default = "d_idle")]
    pub idle_unload_ms: u64,
}

impl EngineConfig {
    /// A fresh config with defaults, model under `models_dir`, applying the
    /// dev/test env overrides `MODELSTAT_LLAMA_MODEL_PATH` / `_CONTEXT`.
    pub fn defaults(models_dir: &Path) -> Self {
        let model_path = std::env::var_os("MODELSTAT_LLAMA_MODEL_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| models_dir.join(MODEL_FILE_NAME));
        let context = std::env::var("MODELSTAT_LLAMA_CONTEXT")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&c| c > 0)
            .unwrap_or(DEFAULT_CONTEXT);
        Self {
            bind: DEFAULT_BIND.to_string(),
            port: DEFAULT_PORT,
            model_path,
            context,
            parallel: DEFAULT_PARALLEL,
            idle_unload_ms: DEFAULT_IDLE_UNLOAD_MS,
        }
    }

    /// The model download URL — `MODELSTAT_LLAMA_MODEL_URL` overrides the pin.
    pub fn model_url() -> String {
        std::env::var("MODELSTAT_LLAMA_MODEL_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL_URL.to_string())
    }

    /// The download spec for the pinned model into `self.model_path`.
    pub fn download_spec(&self) -> DownloadSpec {
        DownloadSpec {
            url: Self::model_url(),
            dest: self.model_path.clone(),
            expected_sha256: None, // pinned per release (M7)
            size_label: Some(MODEL_SIZE_LABEL.to_string()),
            label: "Qwen3.5-4B (Q4_K_M)".to_string(),
        }
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Write `summarizer.json` atomically (tmp + rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_summarizer_json() {
        let dir = std::env::temp_dir().join(format!("modelstat-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("summarizer.json");
        let cfg = EngineConfig::defaults(Path::new("/models"));
        cfg.save(&path).unwrap();
        let back = EngineConfig::load(&path).unwrap();
        assert_eq!(cfg, back);
        assert_eq!(back.port, 4321);
        assert_eq!(back.context, 4096);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_place_model_under_models_dir() {
        // (No env override set in this test process.)
        if std::env::var_os("MODELSTAT_LLAMA_MODEL_PATH").is_none() {
            let cfg = EngineConfig::defaults(Path::new("/m"));
            assert_eq!(cfg.model_path, PathBuf::from("/m/Qwen3.5-4B-Q4_K_M.gguf"));
        }
    }
}
