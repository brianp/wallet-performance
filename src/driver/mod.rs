pub mod new_wallet_driver;
pub mod old_wallet_driver;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Unified balance representation across both wallet implementations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletBalance {
    pub available: u64,
    pub pending_incoming: u64,
    pub pending_outgoing: u64,
    pub locked: u64,
}

/// Minimal transaction result returned after a send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResult {
    /// Transaction identifier (format varies by wallet)
    pub tx_id: String,
    /// Whether the transaction was accepted
    pub accepted: bool,
    /// Optional error message if not accepted
    pub error: Option<String>,
    /// Fee charged for this transaction (MicroMinotari), if known
    pub fee: Option<u64>,
}

/// Status of a completed transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionStatus {
    pub tx_id: String,
    pub status: String,
    pub mined_height: Option<u64>,
    pub confirmations: Option<u64>,
}

/// UTXO information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtxoInfo {
    pub value: u64,
    pub status: String,
}

/// Common interface abstracting both wallet implementations.
///
/// Each method returns `anyhow::Result` for flexibility across gRPC and HTTP error types.
#[async_trait]
pub trait WalletDriver: Send + Sync {
    /// Human-readable name for this driver (e.g., "old_wallet", "new_wallet")
    fn name(&self) -> &str;

    /// Send a transaction to a single recipient.
    async fn send_transaction(&self, recipient: &str, amount: u64) -> anyhow::Result<SendResult>;

    /// Get the current wallet balance.
    async fn get_balance(&self) -> anyhow::Result<WalletBalance>;

    /// Get the status of a specific transaction.
    async fn get_transaction_status(&self, tx_id: &str) -> anyhow::Result<TransactionStatus>;

    /// Split coins: create `count` UTXOs of `amount_per_split` each.
    /// Returns the transaction ID of the split operation.
    async fn split_coins(&self, amount_per_split: u64, count: u64) -> anyhow::Result<SendResult>;

    /// List all UTXOs (or a summary).
    async fn list_utxos(&self) -> anyhow::Result<Vec<UtxoInfo>>;

    /// Get the wallet's receive address.
    async fn get_address(&self) -> anyhow::Result<String>;

    /// Get completed transactions (recent).
    async fn get_completed_transactions(&self) -> anyhow::Result<Vec<TransactionStatus>>;

    /// Wait for all pending transactions to confirm.
    /// Returns once balance is stable for `stable_duration_secs`.
    async fn wait_for_balance_stable(&self, timeout_secs: u64) -> anyhow::Result<WalletBalance>;

    /// Sync wallet with the blockchain. No-op for wallets that sync automatically (e.g. gRPC).
    async fn sync_blockchain(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
