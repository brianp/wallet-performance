use async_trait::async_trait;

use super::Scenario;
use crate::address_pool::AddressPool;
use crate::config::WalletConfig;
use crate::driver::WalletDriver;
use crate::load::{LoadGenerator, LoadPattern};
use crate::metrics::MetricsCollector;

/// Scenario 3: Bidirectional Stress (Send + Receive Simultaneously).
///
/// Simulates a real-world hot wallet that is both paying out and receiving deposits.
/// Budget: 20,000 tXTM (10k per wallet, recycled).
///
/// ~90 sends over 30 minutes ramp. UTXOs pre-split by ensure_utxos.
pub struct BidirectionalScenario;

#[async_trait]
impl Scenario for BidirectionalScenario {
    fn name(&self) -> &str {
        "bidirectional"
    }

    fn budget(&self) -> u64 {
        20_000_000_000 // 20,000 tXTM
    }

    fn required_utxos(&self) -> u64 {
        100 // ~90 ramp sends + headroom
    }

    async fn run(
        &self,
        wallet: &dyn WalletDriver,
        address_pool: &AddressPool,
        _config: &WalletConfig,
        collector: &MetricsCollector,
    ) -> anyhow::Result<()> {
        println!("\n  [{}] SCENARIO: Bidirectional Stress", wallet.name());
        println!("  Ramp 1 tx/min -> 5 tx/min over 30 MINUTES (~90 sends)");
        println!("  Steps: 1/min for 6m, 2/min for 6m, 3/min for 6m, 4/min for 6m, 5/min for 6m");
        println!("  (UTXOs pre-split by ensure_utxos)");
        println!("  Estimated total time: ~30 min\n");

        println!("  [{}] Ramping send rate over 30 minutes...", wallet.name());
        let pattern = LoadPattern::Ramp {
            start_tps: 1.0 / 60.0,   // 1 tx/min
            max_tps: 5.0 / 60.0,     // 5 tx/min
            step_tps: 1.0 / 60.0,    // increase by 1 tx/min
            step_interval_secs: 360, // every 6 minutes
            duration_secs: 1800,     // 30 minutes
        };

        let amount = 50_000_000; // 50 tXTM per tx
        LoadGenerator::execute(
            &pattern,
            wallet,
            address_pool,
            amount,
            self.name(),
            collector,
        )
        .await?;

        println!("  [{}] Bidirectional scenario complete.\n", wallet.name());
        Ok(())
    }
}
