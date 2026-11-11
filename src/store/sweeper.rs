use std::time::Duration;

use tokio::task::JoinHandle;

use crate::config;
use crate::store::Store;

/// Stops the background sweep when the application state is dropped.
pub struct Sweeper(JoinHandle<()>);

impl Drop for Sweeper {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Periodically removes expired conversations, evicts least recently used ones while the store
/// is over its size cap, and drops settings nothing refers to.
pub fn spawn(store: Store, config: &config::Store) -> Sweeper {
    let ttl_ms = i64::try_from(config.ttl_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    let max_bytes = config.max_bytes;
    let every = Duration::from_secs(config.sweep_interval_secs.max(1));

    Sweeper(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);

        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;

            match store.sweep(ttl_ms, max_bytes).await {
                Ok(stats) if stats.expired + stats.evicted > 0 => {
                    tracing::info!(
                        target: "trout::store",
                        expired = stats.expired,
                        evicted = stats.evicted,
                        "sweep"
                    );
                }
                Ok(_) => {}
                Err(_) => tracing::warn!(target: "trout::store", "sweep failed"),
            }
        }
    }))
}
