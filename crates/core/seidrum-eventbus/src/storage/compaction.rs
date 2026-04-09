use super::EventStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, info};

/// Background compaction task that periodically removes old delivered events.
pub struct CompactionTask {
    store: Arc<dyn EventStore>,
    interval: Duration,
    retention: Duration,
    shutdown_rx: watch::Receiver<bool>,
}

impl CompactionTask {
    pub fn new(
        store: Arc<dyn EventStore>,
        interval: Duration,
        retention: Duration,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            store,
            interval,
            retention,
            shutdown_rx,
        }
    }

    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(self.interval);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match self.store.compact(self.retention).await {
                        Ok(removed) => {
                            if removed > 0 {
                                info!(removed = removed, "compaction completed");
                            }
                        }
                        Err(e) => {
                            error!(error = %e, "compaction failed");
                        }
                    }
                }
                result = self.shutdown_rx.changed() => {
                    if result.is_ok() && *self.shutdown_rx.borrow() {
                        info!("compaction task shutting down");
                        break;
                    }
                    if result.is_err() {
                        // Sender dropped — shut down.
                        break;
                    }
                }
            }
        }
    }
}
