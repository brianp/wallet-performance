use anyhow::Context;
use log::info;
use tokio::time::{sleep, Duration};

use crate::driver::WalletDriver;

/// Consolidates UTXOs back into a single UTXO by sending total balance to self.
pub struct Consolidator;

impl Consolidator {
    /// Sweep all funds to a single UTXO.
    pub async fn consolidate(
        wallet: &dyn WalletDriver,
        confirmation_wait_secs: u64,
    ) -> anyhow::Result<()> {
        let wallet_name = wallet.name();
        let address = wallet.get_address().await?;
        let balance = wallet.get_balance().await?;

        if balance.available == 0 {
            info!("[{}] No funds to consolidate", wallet_name);
            return Ok(());
        }

        // Reserve some for fees (estimate: 10,000 MicroMinotari)
        let fee_reserve = 10_000;
        let send_amount = balance.available.saturating_sub(fee_reserve);

        if send_amount == 0 {
            info!(
                "[{}] Balance too low to consolidate (available={})",
                wallet_name, balance.available
            );
            return Ok(());
        }

        info!(
            "[{}] Consolidating: sending {} to self (keeping {} for fees)",
            wallet_name, send_amount, fee_reserve
        );

        let result = wallet
            .send_transaction(&address, send_amount)
            .await
            .context("Consolidation send failed")?;

        if !result.accepted {
            anyhow::bail!("Consolidation rejected: {:?}", result.error);
        }

        info!(
            "[{}] Consolidation tx submitted: {} (fee={:?}). Waiting for confirmation...",
            wallet_name, result.tx_id, result.fee
        );

        // Poll for tx to be mined, with fallback to simple wait
        let poll_interval = Duration::from_secs(30);
        let deadline = std::time::Instant::now() + Duration::from_secs(confirmation_wait_secs);

        while std::time::Instant::now() < deadline {
            sleep(poll_interval).await;

            match wallet.get_transaction_status(&result.tx_id).await {
                Ok(status) => {
                    if status.mined_height.is_some() {
                        info!(
                            "[{}] Consolidation tx {} mined at height {:?}",
                            wallet_name, result.tx_id, status.mined_height
                        );
                        break;
                    }
                    info!(
                        "[{}] Tx {} status: {} (waiting for mining...)",
                        wallet_name, result.tx_id, status.status
                    );
                }
                Err(_) => {
                    // Transaction may not be found yet, continue waiting
                }
            }
        }

        let final_balance = wallet.get_balance().await?;
        info!(
            "[{}] Post-consolidation balance: available={}",
            wallet_name, final_balance.available
        );

        Ok(())
    }
}
