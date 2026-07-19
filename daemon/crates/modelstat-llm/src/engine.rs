//! The engine lifecycle (feature §10.2): lazy-load (503 while loading), a single
//! serialized inference worker, idle-unload, download-on-first-use, and
//! drain-on-shutdown. Generic over a [`Backend`]; the server holds a non-generic
//! `Engine`.
//!
//! The worker is a dedicated OS thread (inference is blocking); the async server
//! hands it jobs and awaits results over a oneshot. State transitions are the
//! contract the protocol server reads (`Loaded` → serve; anything else → 503).

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::oneshot;

use crate::backend::{strip_think, Backend, GenParams};
use crate::config::EngineConfig;

/// The engine's load state (the protocol server maps everything but `Loaded` to
/// a 503 + Retry-After).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineState {
    Unloaded,
    Downloading,
    Loading,
    Loaded,
    Failed(String),
}

/// The result of a `/v1/complete` attempt.
pub enum CompleteOutcome {
    Ready(String),
    /// Still (down)loading — server responds 503 + Retry-After; the client retries.
    Loading,
    Failed(String),
}

enum Job {
    Load,
    Complete {
        params: GenParams,
        resp: oneshot::Sender<Result<String, String>>,
    },
    Shutdown {
        done: oneshot::Sender<()>,
    },
}

/// The inference engine — one per process.
pub struct Engine {
    state: Arc<Mutex<EngineState>>,
    job_tx: mpsc::Sender<Job>,
    backend_name: &'static str,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Engine {
    /// Start the engine over `backend`. The model is NOT loaded yet — the first
    /// `complete` triggers a background download+load (§10.2 lazy load).
    pub fn new<B: Backend>(backend: B, config: EngineConfig) -> Self {
        let backend_name = backend.backend_name();
        let state = Arc::new(Mutex::new(EngineState::Unloaded));
        let (tx, rx) = mpsc::channel::<Job>();
        let worker = std::thread::Builder::new()
            .name("modelstat-engine".into())
            .spawn({
                let state = state.clone();
                move || worker_loop(backend, config, state, rx)
            })
            .expect("spawn engine worker");
        Engine {
            state,
            job_tx: tx,
            backend_name,
            worker: Mutex::new(Some(worker)),
        }
    }

    /// The backend name reported by `/healthz` (`"metal"`|`"cpu"`).
    pub fn backend_name(&self) -> &'static str {
        self.backend_name
    }

    pub fn is_loaded(&self) -> bool {
        matches!(*self.state.lock().unwrap(), EngineState::Loaded)
    }

    pub fn state(&self) -> EngineState {
        self.state.lock().unwrap().clone()
    }

    /// Attempt a completion. `Loaded` → enqueue on the serialized worker and
    /// await; otherwise trigger a background load and report `Loading` (→ 503).
    pub async fn complete(&self, params: GenParams) -> CompleteOutcome {
        match self.state() {
            EngineState::Loaded => {
                let (tx, rx) = oneshot::channel();
                if self.job_tx.send(Job::Complete { params, resp: tx }).is_err() {
                    return CompleteOutcome::Failed("engine worker is gone".into());
                }
                match rx.await {
                    Ok(Ok(text)) => CompleteOutcome::Ready(text),
                    Ok(Err(e)) => CompleteOutcome::Failed(e),
                    Err(_) => CompleteOutcome::Failed("engine worker dropped the job".into()),
                }
            }
            EngineState::Unloaded | EngineState::Failed(_) => {
                self.trigger_load();
                CompleteOutcome::Loading
            }
            EngineState::Downloading | EngineState::Loading => CompleteOutcome::Loading,
        }
    }

    /// Kick off a background load exactly once (idempotent across concurrent
    /// requests). A prior `Failed` state re-triggers, so load failures self-heal.
    fn trigger_load(&self) {
        let mut st = self.state.lock().unwrap();
        if matches!(*st, EngineState::Unloaded | EngineState::Failed(_)) {
            *st = EngineState::Loading;
            drop(st);
            let _ = self.job_tx.send(Job::Load);
        }
    }

    /// Refuse new work, drain the in-flight job (≤8s), free the backend, and
    /// stop the worker (feature §6.8/§10.2 teardown).
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.job_tx.send(Job::Shutdown { done: tx }).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(8), rx).await;
        }
        if let Some(handle) = self.worker.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

fn set_state(state: &Arc<Mutex<EngineState>>, next: EngineState) {
    *state.lock().unwrap() = next;
}

fn worker_loop<B: Backend>(
    mut backend: B,
    config: EngineConfig,
    state: Arc<Mutex<EngineState>>,
    rx: mpsc::Receiver<Job>,
) {
    let mut loaded = false;
    let idle = (config.idle_unload_ms > 0).then(|| Duration::from_millis(config.idle_unload_ms));

    loop {
        // When loaded with a finite idle window, wake to unload after inactivity.
        let job = match (loaded, idle) {
            (true, Some(dur)) => match rx.recv_timeout(dur) {
                Ok(j) => j,
                Err(RecvTimeoutError::Timeout) => {
                    backend.unload();
                    loaded = false;
                    set_state(&state, EngineState::Unloaded);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
            _ => match rx.recv() {
                Ok(j) => j,
                Err(_) => break,
            },
        };

        match job {
            Job::Load => {
                if loaded {
                    continue; // a queued duplicate — already up
                }
                if !config.model_path.exists() {
                    set_state(&state, EngineState::Downloading);
                    if let Err(e) = download_model(&config) {
                        set_state(&state, EngineState::Failed(e));
                        continue;
                    }
                }
                set_state(&state, EngineState::Loading);
                match backend.load(&config.model_path, config.context) {
                    Ok(()) => {
                        loaded = true;
                        set_state(&state, EngineState::Loaded);
                    }
                    Err(e) => set_state(&state, EngineState::Failed(e)),
                }
            }
            Job::Complete { params, resp } => {
                if !loaded {
                    let _ = resp.send(Err("engine not loaded".into()));
                    continue;
                }
                let result = backend.generate(&params).map(|raw| strip_think(&raw));
                let _ = resp.send(result);
            }
            Job::Shutdown { done } => {
                if loaded {
                    backend.unload();
                }
                let _ = done.send(());
                break;
            }
        }
    }
}

/// Download the pinned model on a throwaway current-thread runtime (the worker is
/// a blocking OS thread). Failures self-heal — the next request retries.
fn download_model(config: &EngineConfig) -> Result<(), String> {
    let spec = config.download_spec();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let client = reqwest::Client::new();
    let sink = modelstat_download::TtyProgress::new("Qwen3.5-4B");
    rt.block_on(modelstat_download::download(&client, &spec, &sink))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockBackend;
    use std::path::PathBuf;
    use std::time::Duration;

    fn params() -> GenParams {
        GenParams {
            system: "sys".into(),
            user: "hello".into(),
            temperature: 0.2,
            max_tokens: 1024,
            top_k: Some(3),
        }
    }

    fn present_model() -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "modelstat-engine-{}-{}",
            std::process::id(),
            // vary by a monotonic-ish nonce so parallel tests don't collide
            RAND.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("model.gguf");
        std::fs::write(&model, b"pretend gguf").unwrap();
        (dir, model)
    }

    static RAND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn config(model: PathBuf, idle_ms: u64) -> EngineConfig {
        EngineConfig {
            bind: "127.0.0.1".into(),
            port: 4321,
            model_path: model,
            context: 4096,
            parallel: 1,
            idle_unload_ms: idle_ms,
        }
    }

    async fn wait_loaded(engine: &Engine) {
        for _ in 0..200 {
            if engine.is_loaded() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("engine never loaded: {:?}", engine.state());
    }

    #[tokio::test]
    async fn lazy_load_then_complete_strips_think() {
        let (dir, model) = present_model();
        let engine = Engine::new(MockBackend::ready(), config(model, 0));
        // First request triggers a background load and reports Loading.
        assert!(matches!(engine.complete(params()).await, CompleteOutcome::Loading));
        wait_loaded(&engine).await;
        match engine.complete(params()).await {
            CompleteOutcome::Ready(t) => {
                assert_eq!(t, "a concise redacted summary"); // <think> stripped
            }
            other => panic!("expected Ready, got {}", outcome_name(&other)),
        }
        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn load_failure_is_reported_and_retries() {
        let (dir, model) = present_model();
        let engine = Engine::new(MockBackend::failing_load(), config(model, 0));
        assert!(matches!(engine.complete(params()).await, CompleteOutcome::Loading));
        // Wait for the load attempt to fail.
        for _ in 0..200 {
            if matches!(engine.state(), EngineState::Failed(_)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(matches!(engine.state(), EngineState::Failed(_)));
        // A subsequent request re-triggers the load (self-heal) → Loading again.
        assert!(matches!(engine.complete(params()).await, CompleteOutcome::Loading));
        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn generate_failure_is_a_failed_outcome() {
        let (dir, model) = present_model();
        let engine = Engine::new(MockBackend::failing_generate(), config(model, 0));
        engine.complete(params()).await; // trigger load
        wait_loaded(&engine).await;
        match engine.complete(params()).await {
            CompleteOutcome::Failed(e) => assert!(e.contains("mock inference failure")),
            other => panic!("expected Failed, got {}", outcome_name(&other)),
        }
        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn idle_unload_returns_to_unloaded() {
        let (dir, model) = present_model();
        let engine = Engine::new(MockBackend::ready(), config(model, 30)); // 30ms idle
        engine.complete(params()).await;
        wait_loaded(&engine).await;
        // After the idle window with no work, the worker unloads.
        for _ in 0..200 {
            if matches!(engine.state(), EngineState::Unloaded) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(engine.state(), EngineState::Unloaded);
        engine.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn outcome_name(o: &CompleteOutcome) -> &'static str {
        match o {
            CompleteOutcome::Ready(_) => "Ready",
            CompleteOutcome::Loading => "Loading",
            CompleteOutcome::Failed(_) => "Failed",
        }
    }
}
