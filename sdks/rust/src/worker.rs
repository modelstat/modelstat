//! The background worker: the only place redaction, batching, and network I/O
//! happen. It drains the buffer on a timer or when a batch fills, converts
//! captured calls into a wire batch, and ships it via the [`Transport`].

use crate::capture::{self, LlmCall};
use crate::config::Config;
use crate::transport::Transport;
use crate::Msg;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

/// Spawn the worker task. Must be called from within a Tokio runtime.
pub(crate) fn spawn(cfg: Config, mut rx: mpsc::Receiver<Msg>, transport: Arc<dyn Transport>) {
    tokio::spawn(async move {
        let mut buf: Vec<LlmCall> = Vec::with_capacity(cfg.flush_max_batch);
        let mut seq: u64 = 0;
        let mut ticker = tokio::time::interval(cfg.flush_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The first tick fires immediately; skip it so an idle SDK is silent.
        ticker.tick().await;

        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(Msg::Call(call)) => {
                        buf.push(*call);
                        if buf.len() >= cfg.flush_max_batch {
                            flush(&cfg, &mut buf, &mut seq, transport.as_ref()).await;
                        }
                    }
                    Some(Msg::Drain(ack)) => {
                        flush(&cfg, &mut buf, &mut seq, transport.as_ref()).await;
                        let _ = ack.send(());
                    }
                    None => {
                        // All senders dropped: final flush and exit.
                        flush(&cfg, &mut buf, &mut seq, transport.as_ref()).await;
                        break;
                    }
                },
                _ = ticker.tick() => {
                    flush(&cfg, &mut buf, &mut seq, transport.as_ref()).await;
                }
            }
        }
    });
}

/// Convert and ship the buffered calls. Retries once on failure, then drops the
/// batch loudly (in `LocalDaemon` mode the daemon owns durable retry; remote
/// durability is a follow-up — see the crate README).
async fn flush(cfg: &Config, buf: &mut Vec<LlmCall>, seq: &mut u64, transport: &dyn Transport) {
    if buf.is_empty() {
        return;
    }
    let batch = capture::build_batch(cfg, buf.drain(..), seq);

    for attempt in 0..2u8 {
        match transport.send(&batch).await {
            Ok(()) => return,
            Err(e) if attempt == 0 => {
                eprintln!("modelstat: send failed (retrying once): {e}");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => {
                eprintln!(
                    "modelstat: dropping batch of {} events after retry: {e}",
                    batch.events.len()
                );
            }
        }
    }
}
