use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use log::info;

use super::Scenario;
use crate::address_pool::AddressPool;
use crate::config::WalletConfig;
use crate::driver::WalletDriver;
use crate::metrics::recorder::TransactionRecord;
use crate::metrics::MetricsCollector;

/// Scenario 5: Concurrent Lock Contention.
///
/// Tests UTXO locking under rapid sequential API access.
/// Budget: 10,000 tXTM.
///
/// For the new wallet, this measures SQLite contention and UTXO selection
/// under rapid sequential sends (the DB-level locking is the bottleneck).
///
/// For the old wallet, this tests the internal output manager service
/// contention under rapid sequential Transfer calls.
pub struct LockContentionScenario;

#[async_trait]
impl Scenario for LockContentionScenario {
    fn name(&self) -> &str {
        "lock_contention"
    }

    fn budget(&self) -> u64 {
        10_000_000_000 // 10,000 tXTM
    }

    fn required_utxos(&self) -> u64 {
        100 // 10 + 25 + 50 rapid sends + headroom
    }

    async fn run(
        &self,
        wallet: &dyn WalletDriver,
        address_pool: &AddressPool,
        _config: &WalletConfig,
        collector: &MetricsCollector,
    ) -> anyhow::Result<()> {
        println!("\n  [{}] SCENARIO: Lock Contention", wallet.name());
        println!("  Batch 1: 10 rapid sequential sends");
        println!("  Batch 2: 25 rapid sequential sends");
        println!("  Batch 3: 50 rapid sequential sends");
        println!("  10s cooldown between batches");
        println!("  Estimated total time: ~2 min\n");

        let batch_sizes = [10, 25, 50];
        let send_amount = 50_000_000u64; // 50 tXTM

        for batch_size in batch_sizes {
            println!("  [{}] Batch: {} rapid sends...", wallet.name(), batch_size);

            for _ in 0..batch_size {
                let start = Utc::now();
                let timer = Instant::now();

                let result = wallet
                    .send_transaction(address_pool.next_address(), send_amount)
                    .await;
                let duration = timer.elapsed();
                let end = Utc::now();

                let (tx_id, accepted, error, fee) = match result {
                    Ok(r) => (r.tx_id, r.accepted, r.error, r.fee),
                    Err(e) => (String::new(), false, Some(e.to_string()), None),
                };

                collector.record_transaction(TransactionRecord {
                    wallet: wallet.name().to_string(),
                    scenario: format!("lock_contention_{}", batch_size),
                    tx_id,
                    start_time: start,
                    end_time: end,
                    duration_ms: duration.as_millis() as u64,
                    accepted,
                    error,
                    amount: send_amount,
                    fee,
                    tx_type: "sequential_send".to_string(),
                });
            }

            // Cool down between rounds
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        }

        // Verify balance after contention
        let balance = wallet.get_balance().await?;
        info!(
            "[{}] Post-contention balance: available={}, locked={}",
            wallet.name(),
            balance.available,
            balance.locked
        );

        Ok(())
    }
}
