//! How a built batch leaves the worker. The [`Transport`] trait lets tests run
//! the whole pipeline in-process (via [`FakeTransport`]) and lets the daemon /
//! server paths share one worker.

use crate::config::Config;
use crate::wire::IngestBatch;
use async_trait::async_trait;
use std::sync::Mutex;

/// A transport error. The worker retries once, then drops the batch (the local
/// daemon, in `LocalDaemon` mode, owns durable retry).
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("http status {0}")]
    Status(u16),
    #[error("transport: {0}")]
    Other(String),
}

/// Ships a built batch to its destination.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, batch: &IngestBatch) -> Result<(), TransportError>;
}

/// In-memory transport for tests: records every batch it is handed.
#[derive(Default)]
pub struct FakeTransport {
    batches: Mutex<Vec<IngestBatch>>,
}

impl FakeTransport {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of every batch sent so far.
    #[must_use]
    pub fn batches(&self) -> Vec<IngestBatch> {
        self.batches.lock().expect("lock").clone()
    }
}

#[async_trait]
impl Transport for FakeTransport {
    async fn send(&self, batch: &IngestBatch) -> Result<(), TransportError> {
        self.batches.lock().expect("lock").push(batch.clone());
        Ok(())
    }
}

/// The real HTTP transport: `POST <endpoint>` with a bearer ingest key.
pub struct HttpTransport {
    client: reqwest::Client,
    endpoint: String,
    bearer: String,
}

impl HttpTransport {
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: cfg.mode.endpoint(),
            bearer: cfg.ingest_key.clone(),
        }
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&self, batch: &IngestBatch) -> Result<(), TransportError> {
        let resp = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.bearer)
            .json(batch)
            .send()
            .await
            .map_err(|e| TransportError::Other(e.to_string()))?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(TransportError::Status(status.as_u16()))
        }
    }
}
