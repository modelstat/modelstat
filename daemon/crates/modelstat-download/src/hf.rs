//! Hugging Face model-bundle download (feature §9.5/§11) — the collector's NER +
//! embedder models. The candle loaders need three files per model —
//! `config.json`, `tokenizer.json`, `model.safetensors` — fetched into the shared
//! cache `<models_dir>/hf/<name>/` that `connect` + the daemon share (survives
//! upgrades).
//!
//! Repos don't all lay these out identically: BGE has `tokenizer.json` at the
//! root, but dslim/bert-base-NER only ships it under `onnx/`. So each model
//! declares a (remote path → local name) map, verified against HF by the
//! `candle_verify` run.
//!
//! Best-effort like every download: a failure leaves the model absent, so the
//! collector runs fail-open (embedder → time-gap) / fail-closed (NER → hold), and
//! self-heals when a later attempt lands the files (§9.5).

use std::path::{Path, PathBuf};

use crate::{download, DownloadError, DownloadSpec, ProgressSink};

/// One file of a model bundle: where it lives in the HF repo vs. what candle
/// expects it named on disk.
pub struct HfFile {
    /// Path within the repo (e.g. `"onnx/tokenizer.json"`).
    pub remote: &'static str,
    /// Local filename in the model dir (e.g. `"tokenizer.json"`).
    pub local: &'static str,
}

/// A Hugging Face model the collector loads via candle.
pub struct HfModel {
    /// `owner/name` HF repo id.
    pub repo: &'static str,
    /// Cache subdirectory under `<models_dir>/hf/`.
    pub dir_name: &'static str,
    /// Human size label for the (dominant) weights file.
    pub weights_size_label: &'static str,
    /// The files to fetch (remote → local).
    pub files: &'static [HfFile],
}

/// BAAI/bge-small-en-v1.5 — 384-dim embeddings (§9.5). Standard root layout.
pub const BGE_SMALL: HfModel = HfModel {
    repo: "BAAI/bge-small-en-v1.5",
    dir_name: "bge-small-en-v1.5",
    weights_size_label: "~130 MB",
    files: &[
        HfFile { remote: "config.json", local: "config.json" },
        HfFile { remote: "tokenizer.json", local: "tokenizer.json" },
        HfFile { remote: "model.safetensors", local: "model.safetensors" },
    ],
};

/// dslim/bert-base-NER — the layer-2 token-classification model (§9.5). Its fast
/// tokenizer only exists under `onnx/` (no root `tokenizer.json`).
pub const BERT_NER: HfModel = HfModel {
    repo: "dslim/bert-base-NER",
    dir_name: "bert-base-NER",
    weights_size_label: "~430 MB",
    files: &[
        HfFile { remote: "config.json", local: "config.json" },
        HfFile { remote: "onnx/tokenizer.json", local: "tokenizer.json" },
        HfFile { remote: "model.safetensors", local: "model.safetensors" },
    ],
};

impl HfModel {
    /// The model's cache dir under `models_dir`.
    pub fn dir(&self, models_dir: &Path) -> PathBuf {
        models_dir.join("hf").join(self.dir_name)
    }

    /// One [`DownloadSpec`] per file. sha256 is pinned per release (M7) — None here.
    pub fn specs(&self, models_dir: &Path) -> Vec<DownloadSpec> {
        let dir = self.dir(models_dir);
        self.files
            .iter()
            .map(|f| DownloadSpec {
                url: format!("https://huggingface.co/{}/resolve/main/{}", self.repo, f.remote),
                dest: dir.join(f.local),
                expected_sha256: None,
                size_label: if f.local == "model.safetensors" {
                    Some(self.weights_size_label.to_string())
                } else {
                    None
                },
                label: format!("{} ({})", self.dir_name, f.local),
            })
            .collect()
    }
}

/// Download every file of `model` into its cache dir, returning the dir. Any file
/// failing aborts (the model isn't usable half-downloaded) — the caller treats
/// that as "model not yet available".
pub async fn download_hf_model(
    client: &reqwest::Client,
    model: &HfModel,
    models_dir: &Path,
    sink: &dyn ProgressSink,
) -> Result<PathBuf, DownloadError> {
    for spec in model.specs(models_dir) {
        download(client, &spec, sink).await?;
    }
    Ok(model.dir(models_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_map_remote_paths_to_local_names() {
        let bge = BGE_SMALL.specs(Path::new("/m"));
        assert_eq!(bge.len(), 3);
        assert_eq!(
            bge[0].url,
            "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main/config.json"
        );
        assert_eq!(bge[2].dest, PathBuf::from("/m/hf/bge-small-en-v1.5/model.safetensors"));

        // NER's tokenizer comes from onnx/ but lands as tokenizer.json.
        let ner = BERT_NER.specs(Path::new("/m"));
        assert_eq!(
            ner[1].url,
            "https://huggingface.co/dslim/bert-base-NER/resolve/main/onnx/tokenizer.json"
        );
        assert_eq!(ner[1].dest, PathBuf::from("/m/hf/bert-base-NER/tokenizer.json"));
    }
}
