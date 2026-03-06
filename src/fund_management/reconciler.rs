use log::{info, warn};

use crate::driver::WalletDriver;
use crate::metrics::MetricsCollector;

/// Verifies balances between scenarios and at end of test.
pub struct Reconciler;

impl Reconciler {
    /// Check that wallet balance matches expected value (initial - cumulative fees).
    /// Returns (actual_balance, expected_balance, matches).
    pub async fn verify_balance(
        wallet: &dyn WalletDriver,
        initial_balance: u64,
        collector: &MetricsCollector,
        tolerance: u64,
    ) -> anyhow::Result<(u64, u64, bool)> {
        let balance = wallet.get_balance().await?;
        let actual = balance.available + balance.pending_incoming;
        let total_fees = collector.total_fees();
        let expected = initial_balance.saturating_sub(total_fees);
        let matches = actual.abs_diff(expected) <= tolerance;

        if matches {
            info!(
                "[{}] Balance reconciled: actual={}, expected={} (initial={} - fees={})",
                wallet.name(),
                actual,
                expected,
                initial_balance,
                total_fees,
            );
        } else {
            warn!(
                "[{}] Balance MISMATCH: actual={}, expected={} (diff={}, tolerance={})",
                wallet.name(),
                actual,
                expected,
                actual.abs_diff(expected),
                tolerance,
            );
        }

        Ok((actual, expected, matches))
    }

    /// Inter-scenario checkpoint: wait for stable balance and verify.
    pub async fn checkpoint(
        wallet: &dyn WalletDriver,
        initial_balance: u64,
        collector: &MetricsCollector,
        timeout_secs: u64,
        tolerance: u64,
    ) -> anyhow::Result<WalletBalance> {
        info!("[{}] Running inter-scenario checkpoint...", wallet.name());

        let balance = wallet.wait_for_balance_stable(timeout_secs).await?;

        let (actual, expected, ok) =
            Self::verify_balance(wallet, initial_balance, collector, tolerance).await?;

        if !ok {
            warn!(
                "[{}] Checkpoint: balance drift detected (actual={}, expected={}). Continuing anyway.",
                wallet.name(),
                actual,
                expected,
            );
        }

        Ok(balance)
    }
}

use crate::driver::WalletBalance;
