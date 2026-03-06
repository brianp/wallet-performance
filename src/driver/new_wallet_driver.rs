use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use log::{debug, info, warn};
use minotari::db::{self, SqlitePool};
use minotari::scan::{ProcessingEvent, ScanMode, ScanStatusEvent, Scanner};
use minotari::transactions::manager::TransactionSender;
use minotari::transactions::one_sided_transaction::Recipient;
use tari_common::configuration::Network;
use tari_common_types::seeds::cipher_seed::CipherSeed;
use tari_common_types::seeds::mnemonic::Mnemonic;
use tari_common_types::seeds::seed_words::SeedWords;
use tari_common_types::tari_address::TariAddress;
use tari_transaction_components::consensus::ConsensusConstantsBuilder;
use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
use tari_transaction_components::key_manager::KeyManager;
use tari_transaction_components::offline_signing::sign_locked_transaction;
use tari_transaction_components::MicroMinotari;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

use super::{SendResult, TransactionStatus, UtxoInfo, WalletBalance, WalletDriver};

const SECONDS_TO_LOCK_UTXO: u64 = 60 * 60; // 1 hour
const SCAN_BATCH_SIZE: u64 = 100;
const SCAN_PROGRESS_LOG_INTERVAL: u64 = 1000; // Log every 1000 blocks

pub struct NewWalletDriver {
    db_path: PathBuf,
    db_pool: SqlitePool,
    account_name: String,
    password: String,
    network: Network,
    seed_words: Vec<String>,
    base_node_url: String,
    confirmation_window: u64,
    /// Tracks whether we've done the initial full sync.
    initial_sync_done: std::sync::atomic::AtomicBool,
}

impl NewWalletDriver {
    pub fn new(
        db_path: PathBuf,
        account_name: String,
        password: String,
        network: Network,
        seed_words: Vec<String>,
        base_node_url: String,
        confirmation_window: u64,
    ) -> anyhow::Result<Self> {
        let db_pool = minotari::init_db(db_path.clone())
            .context("Failed to initialize minotari-cli database")?;

        info!(
            "[new_wallet] Using base node RPC: {} (network: {:?})",
            base_node_url, network
        );

        Ok(Self {
            db_path,
            db_pool,
            account_name,
            password,
            network,
            seed_words,
            base_node_url,
            confirmation_window,
            initial_sync_done: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Run a blockchain scan with progress logging.
    async fn run_scan(&self, mode: ScanMode) -> anyhow::Result<bool> {
        let label = match &mode {
            ScanMode::Full => "full".to_string(),
            ScanMode::Partial { max_blocks } => format!("partial({})", max_blocks),
            ScanMode::Continuous { .. } => "continuous".to_string(),
        };

        let scanner = Scanner::new(
            &self.password,
            &self.base_node_url,
            self.db_path.clone(),
            SCAN_BATCH_SIZE,
            self.confirmation_window,
        )
        .account(&self.account_name)
        .mode(mode);

        let (mut event_rx, scan_future) = scanner.run_with_events();

        // Log progress at reasonable intervals, not every batch
        let last_logged_height = AtomicU64::new(0);
        let log_handle = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    ProcessingEvent::ScanStatus(ScanStatusEvent::Started {
                        from_height, ..
                    }) => {
                        println!("  [scan] Starting from height {}", from_height);
                        last_logged_height.store(from_height, Ordering::Relaxed);
                    }
                    ProcessingEvent::ScanStatus(ScanStatusEvent::Progress {
                        current_height,
                        blocks_scanned,
                        ..
                    }) => {
                        let last = last_logged_height.load(Ordering::Relaxed);
                        if current_height - last >= SCAN_PROGRESS_LOG_INTERVAL {
                            println!(
                                "  [scan] height={}, scanned={}",
                                current_height, blocks_scanned
                            );
                            last_logged_height.store(current_height, Ordering::Relaxed);
                        }
                    }
                    ProcessingEvent::ScanStatus(ScanStatusEvent::Completed {
                        final_height,
                        total_blocks_scanned,
                        ..
                    }) => {
                        println!(
                            "  [scan] Complete: tip={}, blocks={}",
                            final_height, total_blocks_scanned
                        );
                    }
                    ProcessingEvent::ScanStatus(ScanStatusEvent::Paused {
                        last_scanned_height,
                        ..
                    }) => {
                        debug!("[new_wallet] Scan paused at height {}", last_scanned_height);
                    }
                    _ => {}
                }
            }
        });

        let (events, more_blocks) = scan_future
            .await
            .map_err(|e| anyhow!("Scan ({}) failed: {}", label, e))?;

        let _ = log_handle.await;

        if !events.is_empty() {
            info!("[new_wallet] Scan found {} wallet events", events.len());
        }
        Ok(more_blocks)
    }

    /// Full sync from birthday to chain tip. Only needed once.
    pub async fn sync(&self) -> anyhow::Result<()> {
        self.run_scan(ScanMode::Full).await?;
        self.initial_sync_done
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Catch up with any new blocks since last scan.
    pub async fn sync_to_tip(&self) -> anyhow::Result<()> {
        if !self
            .initial_sync_done
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return self.sync().await;
        }
        // After initial sync, a partial scan with a generous limit catches up quickly.
        // The scanner resumes from where it left off (stored in DB).
        let mut more = true;
        while more {
            more = self
                .run_scan(ScanMode::Partial { max_blocks: 5000 })
                .await?;
        }
        Ok(())
    }

    fn create_sender(&self) -> anyhow::Result<TransactionSender> {
        TransactionSender::new(
            self.db_pool.clone(),
            self.account_name.clone(),
            self.password.clone(),
            self.network,
            self.confirmation_window,
        )
    }

    fn derive_key_manager(&self) -> anyhow::Result<KeyManager> {
        let seed_str = self.seed_words.join(" ");
        let mnemonic =
            SeedWords::from_str(&seed_str).map_err(|e| anyhow!("Invalid seed words: {}", e))?;

        let seed = CipherSeed::from_mnemonic(&mnemonic, None)
            .map_err(|e| anyhow!("Failed to derive seed: {}", e))?;

        let wallet_type = WalletType::SeedWords(
            SeedWordsWallet::construct_new(seed)
                .map_err(|e| anyhow!("Failed to construct wallet from seed: {}", e))?,
        );

        KeyManager::new(wallet_type).map_err(|e| anyhow!("Failed to create key manager: {}", e))
    }
}

#[async_trait]
impl WalletDriver for NewWalletDriver {
    fn name(&self) -> &str {
        "new_wallet"
    }

    async fn send_transaction(&self, recipient: &str, amount: u64) -> anyhow::Result<SendResult> {
        let recipient_address = TariAddress::from_base58(recipient)
            .map_err(|e| anyhow!("Invalid recipient address: {}", e))?;

        let mut sender = self.create_sender()?;
        let idempotency_key = Uuid::new_v4().to_string();

        let recipient_details = Recipient {
            address: recipient_address,
            amount: MicroMinotari(amount),
            payment_id: None,
        };

        // Phase 1: Build unsigned transaction (UTXO selection + locking)
        let unsigned_tx = match sender.start_new_transaction(
            idempotency_key,
            recipient_details,
            SECONDS_TO_LOCK_UTXO,
        ) {
            Ok(tx) => tx,
            Err(e) => {
                return Ok(SendResult {
                    tx_id: String::new(),
                    accepted: false,
                    error: Some(format!("Prepare failed: {}", e)),
                    fee: None,
                });
            }
        };

        let fee = unsigned_tx.info.fee.0;

        // Phase 2: Sign with derived keys
        let key_manager = self.derive_key_manager()?;
        let consensus_constants = ConsensusConstantsBuilder::new(self.network).build();

        let signed_tx = match sign_locked_transaction(
            &key_manager,
            consensus_constants,
            self.network,
            unsigned_tx,
        ) {
            Ok(tx) => tx,
            Err(e) => {
                return Ok(SendResult {
                    tx_id: String::new(),
                    accepted: false,
                    error: Some(format!("Sign failed: {}", e)),
                    fee: Some(fee),
                });
            }
        };

        let tx_id = signed_tx.signed_transaction.tx_id.to_string();

        // Phase 3: Broadcast
        match sender
            .finalize_transaction_and_broadcast(signed_tx, self.base_node_url.clone())
            .await
        {
            Ok(_displayed_tx) => Ok(SendResult {
                tx_id,
                accepted: true,
                error: None,
                fee: Some(fee),
            }),
            Err(e) => Ok(SendResult {
                tx_id,
                accepted: false,
                error: Some(format!("Broadcast failed: {}", e)),
                fee: Some(fee),
            }),
        }
    }

    async fn get_balance(&self) -> anyhow::Result<WalletBalance> {
        let conn = self.db_pool.get()?;
        let accounts = minotari::get_accounts(&conn, Some(&self.account_name))?;
        let account = accounts
            .first()
            .ok_or_else(|| anyhow!("Account '{}' not found", self.account_name))?;
        let balance = minotari::get_balance(&conn, account.id)?;

        Ok(WalletBalance {
            available: balance.available.0,
            pending_incoming: balance.unconfirmed.0,
            pending_outgoing: 0,
            locked: balance.locked.0,
        })
    }

    async fn get_transaction_status(&self, tx_id: &str) -> anyhow::Result<TransactionStatus> {
        Ok(TransactionStatus {
            tx_id: tx_id.to_string(),
            status: "unknown".to_string(),
            mined_height: None,
            confirmations: None,
        })
    }

    async fn split_coins(&self, amount_per_split: u64, count: u64) -> anyhow::Result<SendResult> {
        let address = self.get_address().await?;
        let mut last_result = SendResult {
            tx_id: String::new(),
            accepted: false,
            error: Some("No splits performed".to_string()),
            fee: None,
        };

        for _ in 0..count {
            last_result = self.send_transaction(&address, amount_per_split).await?;
            if !last_result.accepted {
                return Ok(last_result);
            }
        }

        Ok(last_result)
    }

    async fn list_utxos(&self) -> anyhow::Result<Vec<UtxoInfo>> {
        let conn = self.db_pool.get()?;
        let accounts = minotari::get_accounts(&conn, Some(&self.account_name))?;
        let account = accounts
            .first()
            .ok_or_else(|| anyhow!("Account '{}' not found", self.account_name))?;

        let outputs = db::fetch_unspent_outputs(&conn, account.id, 0)?;
        Ok(outputs
            .iter()
            .map(|o| UtxoInfo {
                value: o.output.value().0,
                status: "unspent".to_string(),
            })
            .collect())
    }

    async fn get_address(&self) -> anyhow::Result<String> {
        let conn = self.db_pool.get()?;
        let accounts = minotari::get_accounts(&conn, Some(&self.account_name))?;
        let account = accounts
            .first()
            .ok_or_else(|| anyhow!("Account '{}' not found", self.account_name))?;

        let address = account.get_address(self.network, &self.password)?;
        Ok(address.to_base58())
    }

    async fn get_completed_transactions(&self) -> anyhow::Result<Vec<TransactionStatus>> {
        Ok(Vec::new())
    }

    async fn sync_blockchain(&self) -> anyhow::Result<()> {
        self.sync_to_tip().await
    }

    async fn wait_for_balance_stable(&self, timeout_secs: u64) -> anyhow::Result<WalletBalance> {
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        let poll_interval = Duration::from_secs(30);
        let stable_threshold = Duration::from_secs(60);

        // Sync once at the start
        if let Err(e) = self.sync_to_tip().await {
            warn!("Scan during balance wait failed: {}", e);
        }

        let mut last_balance = self.get_balance().await?;
        let mut last_change = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                warn!("Timeout waiting for stable balance on new wallet");
                return Ok(last_balance);
            }

            sleep(poll_interval).await;

            // Quick catch-up scan — usually 0-1 new blocks in 30s
            if let Err(e) = self.sync_to_tip().await {
                warn!("Scan during balance wait failed: {}", e);
            }

            let current = self.get_balance().await?;

            if current.available != last_balance.available
                || current.pending_incoming != last_balance.pending_incoming
            {
                last_balance = current;
                last_change = std::time::Instant::now();
                info!(
                    "[new_wallet] Balance changed: available={}, pending={}",
                    last_balance.available, last_balance.pending_incoming
                );
            } else if last_change.elapsed() > stable_threshold {
                info!(
                    "[new_wallet] Balance stable: available={}",
                    last_balance.available
                );
                return Ok(last_balance);
            }
        }
    }
}
