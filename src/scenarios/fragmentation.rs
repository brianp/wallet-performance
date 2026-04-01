use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use log::info;
use tokio::time::{sleep, Duration};

use super::Scenario;
use crate::address_pool::AddressPool;
use crate::config::WalletConfig;
use crate::driver::WalletDriver;
use crate::fund_management::Splitter;
use crate::metrics::recorder::TransactionRecord;
use crate::metrics::MetricsCollector;

/// Scenario 4: UTXO Fragmentation Stress.
///
/// Tests how wallets handle highly fragmented UTXO sets.
/// Budget: 10,000 tXTM.
pub struct FragmentationScenario;

#[async_trait]
impl Scenario for FragmentationScenario {
    fn name(&self) -> &str {
        "fragmentation"
    }

    fn budget(&self) -> u64 {
        10_000_000_000 // 10,000 tXTM
    }

    fn required_utxos(&self) -> u64 {
        100 // Initial 100 UTXOs, then scenario further fragments them
    }

    async fn run(
        &self,
        wallet: &dyn WalletDriver,
        _address_pool: &AddressPool,
        config: &WalletConfig,
        collector: &MetricsCollector,
    ) -> anyhow::Result<()> {
        println!("\n  [{}] SCENARIO: UTXO Fragmentation", wallet.name());
        println!("  Phase 1: Further split into 400 UTXOs of 20 tXTM (~500 total)");
        println!("  Phase 2: 4 aggregation sends (50, 200, 1000, 5000 tXTM to self)");
        println!("  (Initial 100 UTXOs pre-split by ensure_utxos)");
        println!("  Estimated total time: ~10 min (mostly waiting for confirmations)\n");

        // Phase 1: Further split (ensure_utxos already gave us 100 UTXOs)
        println!(
            "  [{}] Phase 1/2: Splitting into 400 x 20 tXTM...",
            wallet.name()
        );
        Splitter::split(
            wallet,
            20_000_000, // 20 tXTM
            400,        // create 400 more (already have ~100, total ~500)
            50,         // max per round (smaller batches to avoid issues)
            config.confirmation_wait_secs(),
            collector,
        )
        .await?;

        // Phase 2: Send progressively larger amounts requiring more input aggregation
        println!(
            "  [{}] Phase 2/2: Aggregation sends to self...",
            wallet.name()
        );
        let self_address = wallet.get_address().await?;
        let test_amounts = [
            (50_000_000u64, "50_tXTM"),   // ~3 inputs
            (200_000_000, "200_tXTM"),    // ~11 inputs
            (1_000_000_000, "1000_tXTM"), // ~55 inputs
            (5_000_000_000, "5000_tXTM"), // ~275 inputs
        ];

        for (amount, label) in &test_amounts {
            info!(
                "[{}] Phase 2: Sending {} ({}) to self — testing input aggregation",
                wallet.name(),
                label,
                amount
            );

            let start = Utc::now();
            let timer = Instant::now();
            let result = wallet.send_transaction(&self_address, *amount).await;
            let duration = timer.elapsed();
            let end = Utc::now();

            match &result {
                Ok(r) if r.accepted => {
                    info!(
                        "[{}] {} aggregation send took {}ms (tx={})",
                        wallet.name(),
                        label,
                        duration.as_millis(),
                        r.tx_id
                    );
                }
                Ok(r) => {
                    info!(
                        "[{}] {} aggregation send rejected: {:?}",
                        wallet.name(),
                        label,
                        r.error
                    );
                }
                Err(e) => {
                    info!(
                        "[{}] {} aggregation send failed: {}",
                        wallet.name(),
                        label,
                        e
                    );
                }
            }

            let (tx_id, accepted, error, fee) = match result {
                Ok(r) => (r.tx_id, r.accepted, r.error, r.fee),
                Err(e) => (String::new(), false, Some(e.to_string()), None),
            };

            collector.record_transaction(TransactionRecord {
                wallet: wallet.name().to_string(),
                scenario: format!("fragmentation_{}", label),
                tx_id,
                start_time: start,
                end_time: end,
                duration_ms: duration.as_millis() as u64,
                accepted,
                error,
                amount: *amount,
                fee,
                tx_type: "aggregation_send".to_string(),
            });

            // Wait for confirmation before next round
            sleep(Duration::from_secs(config.confirmation_wait_secs())).await;
        }

        println!("  [{}] Fragmentation scenario complete.\n", wallet.name());
        Ok(())
    }
}
