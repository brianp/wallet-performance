use std::sync::Arc;

use chrono::Utc;
use log::debug;
use sysinfo::System;
use tokio::time::{interval, Duration};

use super::recorder::{MetricsCollector, SystemSnapshot};
use crate::driver::WalletDriver;

/// Periodically samples system metrics (CPU, memory) and wallet state.
pub struct SystemMonitor;

impl SystemMonitor {
    /// Start monitoring in the background. Returns a handle to stop it.
    ///
    /// Call `cancel_tx.send(true)` to stop the monitor.
    pub fn start(
        sample_interval_secs: u64,
        collector: Arc<MetricsCollector>,
        wallet: Arc<dyn WalletDriver>,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        let wallet_name = wallet.name().to_string();

        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(sample_interval_secs));
            let mut sys = System::new_all();

            loop {
                tokio::select! {
                    _ = ticker.tick() => {},
                    result = cancel.changed() => {
                        if result.is_ok() && *cancel.borrow() {
                            debug!("System monitor stopping for {}", wallet_name);
                            return;
                        }
                    }
                }

                sys.refresh_all();

                let memory_rss = sys.used_memory();
                let cpu_percent = sys.global_cpu_usage();

                let (utxo_count, balance_available) = match wallet.get_balance().await {
                    Ok(b) => (None, Some(b.available)),
                    Err(_) => (None, None),
                };

                let snapshot = SystemSnapshot {
                    timestamp: Utc::now(),
                    wallet: wallet_name.clone(),
                    memory_rss_bytes: memory_rss,
                    cpu_percent,
                    utxo_count,
                    balance_available,
                };

                collector.record_snapshot(snapshot);
            }
        })
    }
}
