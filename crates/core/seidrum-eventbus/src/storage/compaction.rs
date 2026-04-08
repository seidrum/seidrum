use super::EventStore;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

/// Background compaction task that periodically removes old delivered events.
pub struct CompactionTask {
    store: Arc<dyn EventStore>,
    interval: Duration,
    retention: Duration,
}

impl CompactionTask {
    pub fn new(store: Arc<dyn EventStore>, interval: Duration, retention: Duration) -> Self {
        Self {
            store,
            interval,
            retention,
        }
    }

    pub async fn run(self) {
        let mut interval = tokio::time::interval(self.interval);
        loop {
            interval.tick().await;

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
    }
}
