use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use log::{debug, info, warn};
use minotari::db::{self, SqlitePool};
use minotari::scan::{ProcessingEvent, ScanMode, ScanStatusEvent, Scanner};
use minotari::tasks::unlocker::TransactionUnlocker;
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
use tokio::sync::{mpsc, Notify};
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{SendResult, TransactionStatus, UtxoInfo, WalletBalance, WalletDriver};

const SECONDS_TO_LOCK_UTXO: u64 = 60 * 60 * 2; // 2 hours, matching Universe
const SCAN_PROGRESS_LOG_INTERVAL: u64 = 1000;

/// Shared state between the background scanner and the driver.
struct ScannerState {
    at_tip: AtomicBool,
    tip_reached: Notify,
    last_scanned_height: AtomicU64,
    transactions: Mutex<HashMap<String, TrackedTransaction>>,
}

#[derive(Debug, Clone)]
struct TrackedTransaction {
    status: String,
    mined_height: u64,
    confirmations: u64,
}

pub struct NewWalletDriver {
    db_path: PathBuf,
    db_pool: SqlitePool,
    account_name: String,
    password: String,
    network: Network,
    seed_words: Vec<String>,
    base_node_url: String,
    confirmation_window: u64,
    scanner_state: Arc<ScannerState>,
    cancel_token: CancellationToken,
    _unlocker_shutdown: tokio::sync::broadcast::Sender<()>,
    _scanner_thread: Option<std::thread::JoinHandle<()>>,
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

        // Start the TransactionUnlocker background task, same as the daemon does.
        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        let unlocker = TransactionUnlocker::new(db_pool.clone());
        tokio::spawn(unlocker.run(shutdown_tx.subscribe()));

        // Unlock outputs stuck from completed transactions (the library's unlocker
        // only handles expired PENDING transactions, not COMPLETED ones where the
        // scanner crashed before marking outputs as spent).
        Self::unlock_completed_transaction_outputs(&db_pool)?;

        let scanner_state = Arc::new(ScannerState {
            at_tip: AtomicBool::new(false),
            tip_reached: Notify::new(),
            last_scanned_height: AtomicU64::new(0),
            transactions: Mutex::new(HashMap::new()),
        });

        let cancel_token = CancellationToken::new();

        // Start continuous scanner on a dedicated thread (scan future is !Send
        // because HttpBlockchainScanner is !Send).
        let scanner_thread = {
            let password = password.clone();
            let base_node_url = base_node_url.clone();
            let db_path = db_path.clone();
            let account_name = account_name.clone();
            let cancel_token = cancel_token.clone();
            let state = scanner_state.clone();

            std::thread::Builder::new()
                .name("wallet-scanner".into())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .worker_threads(2)
                        .build()
                        .expect("Failed to create scanner runtime");

                    rt.block_on(async move {
                        loop {
                            if cancel_token.is_cancelled() {
                                break;
                            }

                            let scanner = Scanner::new(
                                &password,
                                &base_node_url,
                                db_path.clone(),
                                25,
                                confirmation_window,
                            )
                            .account(&account_name)
                            .max_error_retries(5)
                            .mode(ScanMode::Continuous {
                                poll_interval: Duration::from_secs(10),
                            })
                            .cancel_token(cancel_token.clone());

                            let (event_rx, scan_future) = scanner.run_with_events();

                            let state_clone = state.clone();
                            let scan_handle = tokio::task::spawn_blocking(move || {
                                tokio::runtime::Handle::current().block_on(scan_future)
                            });

                            let event_handle = tokio::spawn(
                                process_events(event_rx, state_clone)
                            );

                            let scan_result = scan_handle.await;
                            let _ = event_handle.await;

                            match scan_result {
                                Ok(Err(e)) => {
                                    let msg = e.to_string();
                                    if cancel_token.is_cancelled() {
                                        break;
                                    }
                                    warn!("[scanner] Scanner crashed: {}. Restarting in 5s...", msg);
                                    state.at_tip.store(false, Ordering::Release);
                                    tokio::time::sleep(Duration::from_secs(5)).await;
                                }
                                Ok(Ok(_)) => {
                                    if cancel_token.is_cancelled() {
                                        break;
                                    }
                                    info!("[scanner] Scanner exited cleanly. Restarting in 5s...");
                                    tokio::time::sleep(Duration::from_secs(5)).await;
                                }
                                Err(e) => {
                                    warn!("[scanner] Scanner task panicked: {}. Restarting in 5s...", e);
                                    state.at_tip.store(false, Ordering::Release);
                                    tokio::time::sleep(Duration::from_secs(5)).await;
                                }
                            }
                        }
                    });
                })
                .context("Failed to spawn scanner thread")?
        };

        Ok(Self {
            db_path,
            db_pool,
            account_name,
            password,
            network,
            seed_words,
            base_node_url,
            confirmation_window,
            scanner_state,
            cancel_token,
            _unlocker_shutdown: shutdown_tx,
            _scanner_thread: Some(scanner_thread),
        })
    }

    /// Unlock outputs that are LOCKED by completed pending transactions.
    /// This handles the case where the scanner crashed after a transaction was
    /// broadcast but before it could mark the spent outputs.
    fn unlock_completed_transaction_outputs(db_pool: &SqlitePool) -> anyhow::Result<()> {
        let conn = db_pool.get()?;
        let count: i64 = conn.query_row(
            r#"
            SELECT COUNT(*)
            FROM outputs o
            JOIN pending_transactions pt ON o.locked_by_request_id = pt.id
            WHERE o.status = 'LOCKED'
              AND o.deleted_at IS NULL
              AND pt.status = 'COMPLETED'
            "#,
            [],
            |row| row.get(0),
        )?;

        if count > 0 {
            conn.execute(
                r#"
                UPDATE outputs
                SET status = 'UNSPENT', locked_by_request_id = NULL
                WHERE status = 'LOCKED'
                  AND deleted_at IS NULL
                  AND locked_by_request_id IN (
                    SELECT id FROM pending_transactions WHERE status = 'COMPLETED'
                  )
                "#,
                [],
            )?;
            info!(
                "[new_wallet] Unlocked {} outputs from completed transactions",
                count
            );
        }

        Ok(())
    }

    /// Wait for the background continuous scanner to reach the chain tip.
    pub async fn sync_to_tip(&self) -> anyhow::Result<()> {
        if self.scanner_state.at_tip.load(Ordering::Acquire) {
            return Ok(());
        }

        let timeout = Duration::from_secs(600);
        let start = std::time::Instant::now();

        loop {
            let remaining = timeout
                .checked_sub(start.elapsed())
                .ok_or_else(|| anyhow!("Timed out waiting for scanner to reach tip"))?;

            tokio::select! {
                _ = self.scanner_state.tip_reached.notified() => {
                    if self.scanner_state.at_tip.load(Ordering::Acquire) {
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep(remaining) => {
                    return Err(anyhow!("Timed out waiting for scanner to reach tip"));
                }
            }
        }
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

impl Drop for NewWalletDriver {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

/// Process scanner events, updating shared state.
async fn process_events(
    mut event_rx: mpsc::UnboundedReceiver<ProcessingEvent>,
    state: Arc<ScannerState>,
) {
    let last_logged_height = AtomicU64::new(0);

    while let Some(event) = event_rx.recv().await {
        match event {
            ProcessingEvent::ScanStatus(ScanStatusEvent::Started { from_height, .. }) => {
                state.at_tip.store(false, Ordering::Release);
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
                state.at_tip.store(true, Ordering::Release);
                state
                    .last_scanned_height
                    .store(final_height, Ordering::Release);
                state.tip_reached.notify_waiters();
                info!(
                    "[scanner] At tip: height={}, scanned={}",
                    final_height, total_blocks_scanned
                );
            }
            ProcessingEvent::ScanStatus(ScanStatusEvent::MoreBlocksAvailable {
                last_scanned_height,
                ..
            }) => {
                state.at_tip.store(false, Ordering::Release);
                state
                    .last_scanned_height
                    .store(last_scanned_height, Ordering::Release);
            }
            ProcessingEvent::ScanStatus(ScanStatusEvent::Waiting { .. }) => {
                // Scanner is idle, waiting for next poll — we're at tip
                state.at_tip.store(true, Ordering::Release);
                state.tip_reached.notify_waiters();
            }
            ProcessingEvent::ScanStatus(ScanStatusEvent::Paused {
                last_scanned_height,
                ..
            }) => {
                debug!("[scanner] Paused at height {}", last_scanned_height);
            }
            ProcessingEvent::TransactionsReady(event) => {
                let mut txns = state.transactions.lock().unwrap();
                for tx in &event.transactions {
                    txns.insert(
                        tx.id.to_string(),
                        TrackedTransaction {
                            status: format!("{:?}", tx.status),
                            mined_height: tx.blockchain.block_height,
                            confirmations: tx.blockchain.confirmations,
                        },
                    );
                }
                info!(
                    "[scanner] {} transactions ready at height {:?}",
                    event.transactions.len(),
                    event.block_height
                );
            }
            ProcessingEvent::TransactionsUpdated(event) => {
                let mut txns = state.transactions.lock().unwrap();
                for tx in &event.updated_transactions {
                    txns.insert(
                        tx.id.to_string(),
                        TrackedTransaction {
                            status: format!("{:?}", tx.status),
                            mined_height: tx.blockchain.block_height,
                            confirmations: tx.blockchain.confirmations,
                        },
                    );
                }
            }
            ProcessingEvent::ReorgDetected(event) => {
                state.at_tip.store(false, Ordering::Release);
                warn!(
                    "[scanner] Reorg detected: rolled back {} blocks from height {}",
                    event.blocks_rolled_back, event.reorg_from_height
                );
            }
            _ => {}
        }
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
        let txns = self.scanner_state.transactions.lock().unwrap();
        if let Some(tracked) = txns.get(tx_id) {
            Ok(TransactionStatus {
                tx_id: tx_id.to_string(),
                status: tracked.status.clone(),
                mined_height: Some(tracked.mined_height),
                confirmations: Some(tracked.confirmations),
            })
        } else {
            Ok(TransactionStatus {
                tx_id: tx_id.to_string(),
                status: "unknown".to_string(),
                mined_height: None,
                confirmations: None,
            })
        }
    }

    async fn split_coins(&self, amount_per_split: u64, count: u64) -> anyhow::Result<SendResult> {
        let address = self.get_address().await?;
        let mut last_result = SendResult {
            tx_id: String::new(),
            accepted: false,
            error: Some("No splits performed".to_string()),
            fee: None,
        };

        for i in 0..count {
            last_result = self.send_transaction(&address, amount_per_split).await?;
            if !last_result.accepted {
                return Ok(last_result);
            }

            if i + 1 < count {
                info!(
                    "[new_wallet] Split {}/{}: waiting for change UTXO to confirm...",
                    i + 1,
                    count
                );
                self.sync_to_tip().await?;

                let needed = amount_per_split + 10_000;
                let poll_interval = Duration::from_secs(30);
                let max_wait = Duration::from_secs(900);
                let start = std::time::Instant::now();

                loop {
                    let balance = self.get_balance().await?;
                    if balance.available >= needed {
                        break;
                    }
                    if start.elapsed() > max_wait {
                        warn!(
                            "[new_wallet] Timed out waiting for change UTXO. Available: {}, need: {}",
                            balance.available, needed
                        );
                        break;
                    }
                    debug!(
                        "[new_wallet] Available {} < needed {}, waiting...",
                        balance.available, needed
                    );
                    sleep(poll_interval).await;
                    self.sync_to_tip().await?;
                }
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
        let txns = self.scanner_state.transactions.lock().unwrap();
        Ok(txns
            .iter()
            .map(|(id, t)| TransactionStatus {
                tx_id: id.clone(),
                status: t.status.clone(),
                mined_height: Some(t.mined_height),
                confirmations: Some(t.confirmations),
            })
            .collect())
    }

    async fn sync_blockchain(&self) -> anyhow::Result<()> {
        self.sync_to_tip().await
    }

    async fn wait_for_balance_stable(&self, timeout_secs: u64) -> anyhow::Result<WalletBalance> {
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        let poll_interval = Duration::from_secs(30);
        let stable_threshold = Duration::from_secs(60);

        // Wait for initial sync
        self.sync_to_tip().await?;

        let mut last_balance = self.get_balance().await?;
        let mut last_change = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                warn!("Timeout waiting for stable balance on new wallet");
                return Ok(last_balance);
            }

            sleep(poll_interval).await;

            // The continuous scanner keeps the DB up to date in the background,
            // so we just need to re-read the balance.
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
