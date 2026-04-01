use super::Scenario;
use crate::address_pool::AddressPool;
use crate::config::WalletConfig;
use crate::driver::WalletDriver;
use crate::load::{LoadGenerator, LoadPattern};
use crate::metrics::MetricsCollector;
use async_trait::async_trait;

/// Scenario 1: Pool Payout Simulation (Outbound Flood).
///
/// Simulates a mining pool paying out to many recipients rapidly.
/// Budget: 40,000 tXTM.
///
/// In user mode (batch_size=1): 5 TPS × 120s = 600 individual sends + 50 burst.
/// In pool mode (batch_size=50): same recipient count, but batched into fewer txs.
pub struct PoolPayoutScenario {
    batch_size: usize,
}

impl PoolPayoutScenario {
    pub fn new() -> Self {
        Self { batch_size: 1 }
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size;
        self
    }
}

#[async_trait]
impl Scenario for PoolPayoutScenario {
    fn name(&self) -> &str {
        "pool_payout"
    }

    fn budget(&self) -> u64 {
        40_000_000_000 // 40,000 tXTM in MicroMinotari
    }

    fn required_utxos(&self) -> u64 {
        if self.batch_size > 1 {
            20 // Batch mode: fewer, larger UTXOs
        } else {
            700 // 1-to-1 mode: one UTXO per send
        }
    }

    fn split_amount(&self) -> u64 {
        if self.batch_size > 1 {
            // Each UTXO covers batch_size recipients × 50 tXTM + fee headroom
            (self.batch_size as u64 * 50_000_000) + 5_000_000
        } else {
            55_000_000 // 55 tXTM (50 send + 5 fee headroom)
        }
    }

    async fn run(
        &self,
        wallet: &dyn WalletDriver,
        address_pool: &AddressPool,
        _config: &WalletConfig,
        collector: &MetricsCollector,
    ) -> anyhow::Result<()> {
        let mode_label = if self.batch_size > 1 {
            format!("batch of {}", self.batch_size)
        } else {
            "1-to-1".to_string()
        };

        println!(
            "\n  [{}] SCENARIO: Pool Payout ({})",
            wallet.name(),
            mode_label
        );
        println!("  Phase 1: Constant 5 TPS for 2 min (~600 recipients)");
        println!("  Phase 2: Burst 50 recipients as fast as possible");
        println!();

        // Phase 1: Constant rate payout
        println!(
            "  [{}] Phase 1/2: Constant 5 TPS for 120s...",
            wallet.name()
        );
        let constant_pattern = LoadPattern::Constant {
            tps: 5.0,
            duration_secs: 120,
        };

        let amount = 50_000_000; // 50 tXTM per payout
        LoadGenerator::execute_with_batch(
            &constant_pattern,
            wallet,
            address_pool,
            amount,
            self.name(),
            collector,
            self.batch_size,
        )
        .await?;

        // Phase 2: Burst payout
        println!(
            "  [{}] Phase 2/2: Burst sending 50 payouts...",
            wallet.name()
        );
        let burst_pattern = LoadPattern::Burst { count: 50 };
        LoadGenerator::execute_with_batch(
            &burst_pattern,
            wallet,
            address_pool,
            amount,
            &format!("{}_burst", self.name()),
            collector,
            self.batch_size,
        )
        .await?;

        println!("  [{}] Pool payout complete.\n", wallet.name());
        Ok(())
    }
}
