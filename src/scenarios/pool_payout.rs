use async_trait::async_trait;
use super::Scenario;
use crate::address_pool::AddressPool;
use crate::config::WalletConfig;
use crate::driver::WalletDriver;
use crate::load::{LoadGenerator, LoadPattern};
use crate::metrics::MetricsCollector;

/// Scenario 1: Pool Payout Simulation (Outbound Flood).
///
/// Simulates a mining pool paying out to many recipients rapidly.
/// Budget: 40,000 tXTM.
///
/// 5 TPS × 120s = 600 sends + 50 burst = 650 total.
/// Change UTXOs need confirmations before reuse, so we need
/// one pre-split UTXO per send. ensure_utxos handles the split.
pub struct PoolPayoutScenario;

#[async_trait]
impl Scenario for PoolPayoutScenario {
    fn name(&self) -> &str {
        "pool_payout"
    }

    fn budget(&self) -> u64 {
        40_000_000_000 // 40,000 tXTM in MicroMinotari
    }

    fn required_utxos(&self) -> u64 {
        700 // 600 constant + 50 burst + headroom
    }

    fn split_amount(&self) -> u64 {
        55_000_000 // 55 tXTM (50 send + 5 fee headroom)
    }

    async fn run(
        &self,
        wallet: &dyn WalletDriver,
        address_pool: &AddressPool,
        _config: &WalletConfig,
        collector: &MetricsCollector,
    ) -> anyhow::Result<()> {
        println!("\n  [{}] SCENARIO: Pool Payout", wallet.name());
        println!("  Phase 1: Constant 5 TPS for 2 min (~600 sends)");
        println!("  Phase 2: Burst 50 sends as fast as possible");
        println!("  (UTXOs pre-split by ensure_utxos)");
        println!("  Estimated total time: ~3 min\n");

        // Phase 1: Constant rate payout
        println!("  [{}] Phase 1/2: Constant 5 TPS for 120s...", wallet.name());
        let constant_pattern = LoadPattern::Constant {
            tps: 5.0,
            duration_secs: 120,
        };

        let amount = 50_000_000; // 50 tXTM per payout
        LoadGenerator::execute(
            &constant_pattern,
            wallet,
            address_pool,
            amount,
            self.name(),
            collector,
        )
        .await?;

        // Phase 2: Burst payout — send 50 transactions as fast as possible
        println!("  [{}] Phase 2/2: Burst sending 50 payouts...", wallet.name());
        let burst_pattern = LoadPattern::Burst { count: 50 };
        LoadGenerator::execute(
            &burst_pattern,
            wallet,
            address_pool,
            amount,
            &format!("{}_burst", self.name()),
            collector,
        )
        .await?;

        println!("  [{}] Pool payout complete.\n", wallet.name());
        Ok(())
    }
}
