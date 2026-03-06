use std::time::Instant;

use anyhow::Context;
use chrono::Utc;
use log::info;
use tokio::time::{sleep, Duration};

use crate::driver::WalletDriver;
use crate::metrics::recorder::{MetricsCollector, TransactionRecord};

/// Handles UTXO splitting with confirmation waits between rounds.
pub struct Splitter;

impl Splitter {
    /// Poll until both pending_incoming and pending_outgoing are zero.
    async fn wait_for_pending_clear(
        wallet: &dyn WalletDriver,
        max_wait_secs: u64,
    ) -> anyhow::Result<()> {
        let poll_interval = Duration::from_secs(30);
        let start = Instant::now();
        let max_wait = Duration::from_secs(max_wait_secs);

        loop {
            wallet.sync_blockchain().await?;
            let balance = wallet.get_balance().await?;

            if balance.pending_incoming == 0 && balance.pending_outgoing == 0 {
                info!(
                    "[{}] No pending transactions. Available: {} tXTM",
                    wallet.name(),
                    balance.available / 1_000_000,
                );
                return Ok(());
            }

            info!(
                "[{}] Pending in: {} tXTM, out: {} tXTM, available: {} tXTM ({}s elapsed)",
                wallet.name(),
                balance.pending_incoming / 1_000_000,
                balance.pending_outgoing / 1_000_000,
                balance.available / 1_000_000,
                start.elapsed().as_secs(),
            );

            if start.elapsed() > max_wait {
                info!(
                    "[{}] Warning: timed out waiting for pending to clear. Proceeding.",
                    wallet.name(),
                );
                return Ok(());
            }

            sleep(poll_interval).await;
        }
    }

    /// Split funds into `target_count` UTXOs of `amount_per_utxo` each.
    ///
    /// For old wallet: uses native CoinSplit RPC.
    /// For new wallet: uses create_unsigned_transaction with multiple self-recipients.
    ///
    /// Performs cascading splits if `target_count` exceeds the max split per round.
    pub async fn split(
        wallet: &dyn WalletDriver,
        amount_per_utxo: u64,
        target_count: u64,
        max_per_round: u64,
        confirmation_wait_secs: u64,
        collector: &MetricsCollector,
    ) -> anyhow::Result<()> {
        let wallet_name = wallet.name();
        let mut remaining = target_count;
        let mut round = 0;

        info!(
            "[{}] Starting UTXO split: {} UTXOs of {} each",
            wallet_name, target_count, amount_per_utxo
        );

        while remaining > 0 {
            let batch = remaining.min(max_per_round);
            round += 1;

            info!(
                "[{}] Split round {}: creating {} UTXOs of {}",
                wallet_name, round, batch, amount_per_utxo
            );

            let start = Utc::now();
            let timer = Instant::now();

            let result = wallet
                .split_coins(amount_per_utxo, batch)
                .await
                .with_context(|| format!("Split round {} failed", round))?;

            let duration = timer.elapsed();
            let end = Utc::now();

            collector.record_transaction(TransactionRecord {
                wallet: wallet_name.to_string(),
                scenario: "utxo_split".to_string(),
                tx_id: result.tx_id.clone(),
                start_time: start,
                end_time: end,
                duration_ms: duration.as_millis() as u64,
                accepted: result.accepted,
                error: result.error.clone(),
                amount: amount_per_utxo * batch,
                fee: result.fee,
                tx_type: format!("split_round_{}", round),
            });

            if !result.accepted {
                anyhow::bail!("Split round {} rejected: {:?}", round, result.error);
            }

            remaining -= batch;

            // Wait for split outputs to confirm before next round (or before returning)
            // Without this, change + new UTXOs are pending and the next round fails
            info!(
                "[{}] Split round {} broadcast. Waiting for confirmations... ({} remaining)",
                wallet_name, round, remaining
            );
            Self::wait_for_pending_clear(wallet, confirmation_wait_secs * 10).await?;

            let balance = wallet.get_balance().await?;
            info!(
                "[{}] Balance after round {}: available={}, pending_in={}, pending_out={}",
                wallet_name, round, balance.available,
                balance.pending_incoming, balance.pending_outgoing
            );
        }

        info!(
            "[{}] Split complete: {} rounds, {} total UTXOs",
            wallet_name, round, target_count
        );
        Ok(())
    }
}
