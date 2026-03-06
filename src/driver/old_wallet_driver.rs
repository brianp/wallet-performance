use anyhow::{bail, Context};
use async_trait::async_trait;
use log::{debug, info, warn};
use minotari_wallet_grpc_client::{grpc, WalletGrpcClient};
use tokio::time::{sleep, Duration};
use tonic::transport::Channel;

use super::{SendResult, TransactionStatus, UtxoInfo, WalletBalance, WalletDriver};

/// Driver for the old wallet (minotari_console_wallet) via gRPC.
pub struct OldWalletDriver {
    grpc_addr: String,
    fee_per_gram: u64,
}

impl OldWalletDriver {
    pub fn new(grpc_addr: String, fee_per_gram: u64) -> Self {
        Self {
            grpc_addr,
            fee_per_gram,
        }
    }

    async fn connect(&self) -> anyhow::Result<WalletGrpcClient<Channel>> {
        WalletGrpcClient::connect(&self.grpc_addr)
            .await
            .with_context(|| format!("Failed to connect to old wallet gRPC at {}", self.grpc_addr))
    }
}

#[async_trait]
impl WalletDriver for OldWalletDriver {
    fn name(&self) -> &str {
        "old_wallet"
    }

    async fn send_transaction(&self, recipient: &str, amount: u64) -> anyhow::Result<SendResult> {
        let mut client = self.connect().await?;

        let payment = grpc::PaymentRecipient {
            address: recipient.to_string(),
            amount,
            fee_per_gram: self.fee_per_gram,
            payment_type: 2, // ONE_SIDED_TO_STEALTH_ADDRESS
            raw_payment_id: vec![],
            user_payment_id: None,
        };

        let request = grpc::TransferRequest {
            recipients: vec![payment],
            single_tx: false,
        };

        let response = client.transfer(request).await?.into_inner();

        if let Some(result) = response.results.first() {
            if result.is_success {
                let fee = result.transaction_info.as_ref().map(|info| info.fee);
                Ok(SendResult {
                    tx_id: result.transaction_id.to_string(),
                    accepted: true,
                    error: None,
                    fee,
                })
            } else {
                Ok(SendResult {
                    tx_id: String::new(),
                    accepted: false,
                    error: Some(result.failure_message.clone()),
                    fee: None,
                })
            }
        } else {
            bail!("Transfer returned empty results")
        }
    }

    async fn get_balance(&self) -> anyhow::Result<WalletBalance> {
        let mut client = self.connect().await?;
        let response = client
            .get_balance(grpc::GetBalanceRequest { payment_id: None })
            .await?
            .into_inner();

        Ok(WalletBalance {
            available: response.available_balance,
            pending_incoming: response.pending_incoming_balance,
            pending_outgoing: response.pending_outgoing_balance,
            locked: response.timelocked_balance,
        })
    }

    async fn get_transaction_status(&self, tx_id: &str) -> anyhow::Result<TransactionStatus> {
        let mut client = self.connect().await?;
        let tx_id_u64: u64 = tx_id.parse().context("Invalid tx_id for old wallet")?;

        let response = client
            .get_transaction_info(grpc::GetTransactionInfoRequest {
                transaction_ids: vec![tx_id_u64],
            })
            .await?
            .into_inner();

        if let Some(tx) = response.transactions.first() {
            Ok(TransactionStatus {
                tx_id: tx_id.to_string(),
                status: format!("{}", tx.status),
                mined_height: if tx.mined_in_block_height > 0 {
                    Some(tx.mined_in_block_height)
                } else {
                    None
                },
                confirmations: None,
            })
        } else {
            bail!("Transaction {} not found", tx_id)
        }
    }

    async fn split_coins(&self, amount_per_split: u64, count: u64) -> anyhow::Result<SendResult> {
        let mut client = self.connect().await?;

        let request = grpc::CoinSplitRequest {
            amount_per_split,
            split_count: count,
            fee_per_gram: self.fee_per_gram,
            lock_height: 0,
            payment_id: vec![],
        };

        let response = client.coin_split(request).await?.into_inner();

        Ok(SendResult {
            tx_id: response.tx_id.to_string(),
            accepted: true,
            error: None,
            fee: None, // CoinSplit response doesn't include fee directly
        })
    }

    async fn list_utxos(&self) -> anyhow::Result<Vec<UtxoInfo>> {
        let mut client = self.connect().await?;
        let response = client
            .get_unspent_amounts(grpc::Empty {})
            .await?
            .into_inner();

        Ok(response
            .amount
            .iter()
            .map(|&amount| UtxoInfo {
                value: amount,
                status: "unspent".to_string(),
            })
            .collect())
    }

    async fn get_address(&self) -> anyhow::Result<String> {
        let mut client = self.connect().await?;
        let response = client
            .get_complete_address(grpc::Empty {})
            .await?
            .into_inner();

        Ok(response.one_sided_address_base58)
    }

    async fn get_completed_transactions(&self) -> anyhow::Result<Vec<TransactionStatus>> {
        let mut client = self.connect().await?;
        let mut stream = client
            .get_completed_transactions(grpc::GetCompletedTransactionsRequest {
                payment_id: None,
                block_hash: None,
                block_height: None,
            })
            .await?
            .into_inner();

        let mut transactions = Vec::new();
        while let Some(response) = stream.message().await? {
            if let Some(tx) = response.transaction {
                transactions.push(TransactionStatus {
                    tx_id: tx.tx_id.to_string(),
                    status: format!("{}", tx.status),
                    mined_height: if tx.mined_in_block_height > 0 {
                        Some(tx.mined_in_block_height)
                    } else {
                        None
                    },
                    confirmations: None,
                });
            }
        }

        Ok(transactions)
    }

    async fn wait_for_balance_stable(&self, timeout_secs: u64) -> anyhow::Result<WalletBalance> {
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        let poll_interval = Duration::from_secs(10);
        let stable_threshold = Duration::from_secs(30);

        let mut last_balance = self.get_balance().await?;
        let mut last_change = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                warn!("Timeout waiting for stable balance on old wallet");
                return Ok(last_balance);
            }

            sleep(poll_interval).await;
            let current = self.get_balance().await?;

            if current.available != last_balance.available
                || current.pending_incoming != last_balance.pending_incoming
            {
                last_balance = current;
                last_change = std::time::Instant::now();
                debug!(
                    "Balance changed: available={}, pending={}",
                    last_balance.available, last_balance.pending_incoming
                );
            } else if last_change.elapsed() > stable_threshold {
                info!(
                    "Balance stable for {}s: available={}",
                    stable_threshold.as_secs(),
                    last_balance.available
                );
                return Ok(last_balance);
            }
        }
    }
}
