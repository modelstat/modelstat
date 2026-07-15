//! Run-verify the candle models end to end (feature §9.5): download the BGE
//! embedder + BERT-NER weights, load them, and actually run inference on sample
//! text. This is the "does it really WORK", not just "does it compile", check.
//!
//!   cargo run -p modelstat-pipeline --example candle_verify --features candle
//!
//! Downloads ~560 MB into a temp dir on first run (cached after).

#[cfg(not(feature = "candle"))]
fn main() {
    eprintln!("re-run with `--features candle` to verify the candle models.");
    std::process::exit(2);
}

#[cfg(feature = "candle")]
#[tokio::main]
async fn main() {
    use modelstat_download::{download_hf_model, TtyProgress, BERT_NER, BGE_SMALL};
    use modelstat_pipeline::embed::{CandleEmbedder, Embedder};
    use modelstat_redact::ner::{ner_active, ner_redact, CandleNer};

    let models_dir = std::env::temp_dir().join("modelstat-candle-verify");
    let client = reqwest::Client::new();
    println!("▸ models cache: {}", models_dir.display());

    // ── Embedder ──────────────────────────────────────────────────────────
    let embed_ok = async {
        println!("\n▸ BGE embedder: download + run…");
        let dir = download_hf_model(&client, &BGE_SMALL, &models_dir, &TtyProgress::new("bge-small"))
            .await
            .map_err(|e| format!("download: {e}"))?;
        let embedder = CandleEmbedder::load(&dir).map_err(|e| format!("load: {e}"))?;
        let v = embedder.embed("Fixed the retry logic in the ingest uploader.");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        println!("  dim = {} (expect 384), L2 norm = {norm:.4} (expect ~1.0)", v.len());
        println!("  first values: {:?}", &v[..v.len().min(5)]);
        if v.len() != 384 || (norm - 1.0).abs() > 0.05 {
            return Err(format!("output wrong: dim={}, norm={norm}", v.len()));
        }
        Ok::<(), String>(())
    }
    .await;

    // ── NER redactor ──────────────────────────────────────────────────────
    let ner_ok = async {
        println!("\n▸ BERT-NER redactor: download + run…");
        let dir = download_hf_model(&client, &BERT_NER, &models_dir, &TtyProgress::new("bert-NER"))
            .await
            .map_err(|e| format!("download: {e}"))?;
        let ner = CandleNer::load(&dir).map_err(|e| format!("load: {e}"))?;
        let sample = "Escalate the incident to Katherine Johnson at Globex Corporation.";
        let red = ner_redact(&ner, sample);
        println!("  in : {sample}");
        println!("  out: {}", red.text);
        println!("  counts: {:?}", red.counts);
        let active = ner_active(&ner);
        println!("  ner_active (liveness gate) = {active} (expect true)");
        if red.text.contains("Katherine Johnson") || !active {
            return Err(format!("sentinel PERSON not scrubbed: {}", red.text));
        }
        Ok::<(), String>(())
    }
    .await;

    println!("\n── results ──");
    println!("  embedder: {}", result(&embed_ok));
    println!("  NER:      {}", result(&ner_ok));
    if embed_ok.is_err() || ner_ok.is_err() {
        std::process::exit(1);
    }
    println!("\n✓ candle models run-verified");
}

#[cfg(feature = "candle")]
fn result(r: &Result<(), String>) -> String {
    match r {
        Ok(()) => "✓ OK".to_string(),
        Err(e) => format!("✗ {e}"),
    }
}
