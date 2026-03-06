pub mod bidirectional;
pub mod fragmentation;
pub mod inbound_flood;
pub mod lock_contention;
pub mod pool_payout;

use async_trait::async_trait;

use crate::address_pool::AddressPool;
use crate::config::WalletConfig;
use crate::driver::WalletDriver;
use crate::metrics::MetricsCollector;

/// Common interface for all test scenarios.
#[async_trait]
pub trait Scenario: Send + Sync {
    /// Human-readable scenario name.
    fn name(&self) -> &str;

    /// Budget allocation in MicroMinotari for this scenario.
    fn budget(&self) -> u64;

    /// Minimum number of confirmed UTXOs this scenario needs before starting.
    /// The orchestrator will pre-split if the wallet doesn't have enough.
    fn required_utxos(&self) -> u64 {
        0 // default: no specific requirement
    }

    /// Amount per UTXO when pre-splitting (in MicroMinotari).
    /// Each UTXO should cover one send + fee headroom.
    fn split_amount(&self) -> u64 {
        100_000_000 // default: 100 tXTM
    }

    /// Run the scenario against a wallet driver.
    async fn run(
        &self,
        wallet: &dyn WalletDriver,
        address_pool: &AddressPool,
        config: &WalletConfig,
        collector: &MetricsCollector,
    ) -> anyhow::Result<()>;
}
