//! Hugging Face model-bundle download (feature §9.5/§11) — the collector's NER +
//! embedder models. Each is three files (`config.json`, `tokenizer.json`,
//! `model.safetensors`) fetched into the shared cache
//! `<models_dir>/hf/<name>/` that `connect` + the daemon share (survives
//! upgrades). The candle loaders (`CandleEmbedder`/`CandleNer`) read that dir.
//!
//! Best-effort like every download: a failure leaves the model absent, so the
//! collector runs fail-open (embedder → time-gap) / fail-closed (NER → hold), and
//! self-heals when a later attempt lands the files (§9.5).

use std::path::{Path, PathBuf};

use crate::{download, DownloadError, DownloadSpec, ProgressSink};

/// A Hugging Face model the collector loads via candle.
pub struct HfModel {
    /// `owner/name` HF repo id.
    pub repo: &'static str,
    /// Cache subdirectory under `<models_dir>/hf/`.
    pub dir_name: &'static str,
    /// Human size label for the (dominant) weights file.
    pub weights_size_label: &'static str,
}

/// BAAI/bge-small-en-v1.5 — 384-dim embeddings (§9.5).
pub const BGE_SMALL: HfModel = HfModel {
    repo: "BAAI/bge-small-en-v1.5",
    dir_name: "bge-small-en-v1.5",
    weights_size_label: "~130 MB",
};

/// dslim/bert-base-NER — the layer-2 token-classification model (§9.5).
pub const BERT_NER: HfModel = HfModel {
    repo: "dslim/bert-base-NER",
    dir_name: "bert-base-NER",
    weights_size_label: "~430 MB",
};

/// The files candle needs from an HF BERT repo.
pub const HF_FILES: &[&str] = &["config.json", "tokenizer.json", "model.safetensors"];

impl HfModel {
    /// The model's cache dir under `models_dir`.
    pub fn dir(&self, models_dir: &Path) -> PathBuf {
        models_dir.join("hf").join(self.dir_name)
    }

    /// One [`DownloadSpec`] per file. sha256 is pinned per release (M7) — None here.
    pub fn specs(&self, models_dir: &Path) -> Vec<DownloadSpec> {
        let dir = self.dir(models_dir);
        HF_FILES
            .iter()
            .map(|file| DownloadSpec {
                url: format!("https://huggingface.co/{}/resolve/main/{file}", self.repo),
                dest: dir.join(file),
                expected_sha256: None,
                size_label: if *file == "model.safetensors" {
                    Some(self.weights_size_label.to_string())
                } else {
                    None
                },
                label: format!("{} ({file})", self.dir_name),
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
    fn specs_target_the_shared_cache_and_hf_urls() {
        let specs = BGE_SMALL.specs(Path::new("/m"));
        assert_eq!(specs.len(), 3);
        assert_eq!(
            specs[0].url,
            "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/main/config.json"
        );
        assert_eq!(
            specs[2].dest,
            PathBuf::from("/m/hf/bge-small-en-v1.5/model.safetensors")
        );
        assert!(specs[2].size_label.is_some());
        assert_eq!(BERT_NER.dir(Path::new("/m")), PathBuf::from("/m/hf/bert-base-NER"));
    }
}
