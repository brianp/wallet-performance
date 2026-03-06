use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use log::info;
use tokio::time::{sleep, Duration};

use super::Scenario;
use crate::address_pool::AddressPool;
use crate::config::WalletConfig;
use crate::driver::WalletDriver;
use crate::load::{LoadGenerator, LoadPattern};
use crate::metrics::recorder::TransactionRecord;
use crate::metrics::MetricsCollector;

/// Scenario 2: Inbound Flood (Receive Many Transactions).
///
/// Measures how quickly a wallet detects incoming transactions.
/// Budget: 20,000 tXTM.
///
/// This scenario is asymmetric: one wallet is the "receiver under test",
/// the other is the sender. The orchestrator must run this twice (once per wallet).
/// Here we measure the receiving side's detection latency.
pub struct InboundFloodScenario;

#[async_trait]
impl Scenario for InboundFloodScenario {
    fn name(&self) -> &str {
        "inbound_flood"
    }

    fn budget(&self) -> u64 {
        20_000_000_000 // 20,000 tXTM
    }

    fn required_utxos(&self) -> u64 {
        80 // ~60 Poisson sends + headroom
    }

    async fn run(
        &self,
        wallet: &dyn WalletDriver,
        address_pool: &AddressPool,
        _config: &WalletConfig,
        collector: &MetricsCollector,
    ) -> anyhow::Result<()> {
        println!("\n  [{}] SCENARIO: Inbound Flood", wallet.name());
        println!("  Phase 1: Poisson sends at 0.5 TPS for 2 min (~60 sends)");
        println!("  Phase 2: Monitor for balance changes for 3 min");
        println!("  Estimated total time: ~5 min\n");

        // Phase 1: Send transactions using Poisson pattern to address pool
        println!(
            "  [{}] Phase 1/2: Poisson sends to address pool ({} addresses)...",
            wallet.name(),
            address_pool.len()
        );
        let pattern = LoadPattern::Poisson {
            avg_tps: 0.5, // 1 tx every 2 seconds on average
            duration_secs: 120,
        };
        LoadGenerator::execute(
            &pattern,
            wallet,
            address_pool,
            1_000_000, // 1 tXTM per tx
            &format!("{}_send", self.name()),
            collector,
        )
        .await?;

        // Phase 2: Monitor for inbound transactions
        let initial_balance = wallet.get_balance().await?;
        println!(
            "  [{}] Phase 2/2: Monitoring balance for 3 min (initial: {} µT)...",
            wallet.name(),
            initial_balance.available
        );

        let poll_duration_secs = 180; // 3 minutes
        let poll_interval = Duration::from_secs(15);
        let start = Instant::now();
        let mut detected_count = 0;
        let mut last_tx_count = wallet.get_completed_transactions().await?.len();
        let mut last_available = initial_balance.available;

        info!(
            "[{}] Monitoring for {}s (poll every {}s)",
            wallet.name(),
            poll_duration_secs,
            poll_interval.as_secs()
        );

        while start.elapsed() < Duration::from_secs(poll_duration_secs) {
            sleep(poll_interval).await;

            let current_txs = wallet.get_completed_transactions().await?;
            let new_count = current_txs.len();

            if new_count > last_tx_count {
                let detection_time = Utc::now();
                let new_txs = new_count - last_tx_count;
                detected_count += new_txs;

                // Record each detection event
                for tx in current_txs.iter().skip(last_tx_count) {
                    collector.record_transaction(TransactionRecord {
                        wallet: wallet.name().to_string(),
                        scenario: format!("{}_detect", self.name()),
                        tx_id: tx.tx_id.clone(),
                        start_time: detection_time,
                        end_time: detection_time,
                        duration_ms: start.elapsed().as_millis() as u64,
                        accepted: true,
                        error: None,
                        amount: 0, // Unknown from completed_transactions
                        fee: None,
                        tx_type: "inbound_detect".to_string(),
                    });
                }

                info!(
                    "[{}] Detected {} new inbound transactions (total: {})",
                    wallet.name(),
                    new_txs,
                    detected_count
                );
                last_tx_count = new_count;
            }

            let balance = wallet.get_balance().await?;
            if balance.available != last_available {
                info!(
                    "[{}] Balance update: available={} (was {})",
                    wallet.name(),
                    balance.available,
                    last_available
                );
                last_available = balance.available;
            }
        }

        println!(
            "  [{}] Inbound flood complete: detected {} changes.\n",
            wallet.name(),
            detected_count
        );

        Ok(())
    }
}
