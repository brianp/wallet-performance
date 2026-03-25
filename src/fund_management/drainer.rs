use std::path::Path;

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use log::warn;
use tari_common::configuration::Network;
use tari_common_types::payment_reference::generate_payment_reference;
use tari_common_types::tari_address::TariAddress;
use tari_common_types::types::PrivateKey;
use tari_crypto::tari_utilities::hex::Hex;
use tari_transaction_components::consensus::ConsensusConstantsBuilder;
use tari_transaction_components::key_manager::wallet_types::{SpendWallet, WalletType};
use tari_transaction_components::key_manager::{KeyManager, TransactionKeyManagerInterface};
use tari_transaction_components::offline_signing::sign_locked_transaction;
use tari_transaction_components::MicroMinotari;

use crate::address_pool::AddressPoolEntry;
use minotari_scanning::scanning::BlockchainScanner;
use minotari_scanning::{HttpBlockchainScanner, ScanConfig};
use minotari::db::{self, SqlitePool};
use minotari::http::WalletHttpClient;
use minotari::models::BalanceChange;
use minotari::scan::{ScanMode, Scanner};
use minotari::transactions::manager::TransactionSender;
use minotari::transactions::one_sided_transaction::Recipient;

const SECONDS_TO_LOCK_UTXO: u64 = 60 * 60;
const SCAN_BATCH_SIZE: u64 = 25;
const BIRTHDAY_GENESIS_FROM_UNIX_EPOCH: u64 = 1640995200;

/// Drains funds from address pool sink accounts back to a destination wallet.
pub struct Drainer;

impl Drainer {
    /// Register pool entries as SpendWallet accounts in the wallet DB.
    /// Skips entries that already have an account registered.
    /// Uses the main account's birthday so scanning starts from the right height.
    pub fn register_pool_accounts(
        pool: &SqlitePool,
        entries: &[AddressPoolEntry],
    ) -> anyhow::Result<usize> {
        let conn = pool.get()?;

        let main_accounts = db::get_accounts(&conn, Some("default"))?;
        let birthday = main_accounts
            .first()
            .map(|a| a.birthday as u16)
            .unwrap_or(0);

        let mut registered = 0;

        for entry in entries {
            let account_name = format!("pool_{}", entry.index);

            if db::get_account_by_name(&conn, &account_name)?.is_some() {
                continue;
            }

            let view_key = PrivateKey::from_hex(&entry.view_key_hex)
                .map_err(|_| anyhow!("Invalid view key hex for pool entry {}", entry.index))?;
            let spend_key = PrivateKey::from_hex(&entry.spend_key_hex)
                .map_err(|_| anyhow!("Invalid spend key hex for pool entry {}", entry.index))?;

            let spend_wallet = SpendWallet::new(spend_key, view_key, Some(birthday));
            let wallet_type = WalletType::SpendWallet(spend_wallet);

            db::create_account(&conn, &account_name, &wallet_type, "")?;
            registered += 1;
        }

        Ok(registered)
    }

    /// Bulk scan: create ONE HttpBlockchainScanner with all pool key managers,
    /// fetch blocks once, check all keys in parallel via rayon.
    /// Then insert matched outputs into the correct pool account in the DB.
    ///
    /// This avoids the 3000+ RPC calls that would result from scanning
    /// each of the 1000 pool accounts individually.
    pub async fn scan_pool_accounts(
        db_path: &Path,
        base_node_url: &str,
        entries: &[AddressPoolEntry],
        confirmations: u64,
    ) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let pool = db::init_db(db_path.to_path_buf())?;
        let conn = pool.get()?;

        // First, sync the default account to get the chain tip
        println!("  [drain] Syncing default account to chain tip...");
        let scanner = Scanner::new(
            "",
            base_node_url,
            db_path.to_path_buf(),
            SCAN_BATCH_SIZE,
            confirmations,
        )
        .account("default")
        .mode(ScanMode::Full);

        if let Err(e) = scanner.run().await {
            warn!("[drain] Default account scan failed: {}", e);
        }

        let wallet_client = WalletHttpClient::new(base_node_url.parse()?)?;

        // Start scanning from height 500000 — all pool wallets were created after this
        let start_height: u64 = 522_000;

        let tip = wallet_client.get_tip_info().await?;
        let tip_height = tip
            .metadata
            .as_ref()
            .map(|m| m.best_block_height())
            .unwrap_or(0);
        let blocks_to_scan = tip_height.saturating_sub(start_height);
        println!(
            "  [drain] Scanning {} blocks (height {} to {}) with {} keys...",
            blocks_to_scan,
            start_height,
            tip_height,
            entries.len()
        );

        // Build key managers for all pool entries
        println!(
            "  [drain] Building {} key managers...",
            entries.len()
        );
        let mut key_managers: Vec<KeyManager> = Vec::with_capacity(entries.len());
        for (i, entry) in entries.iter().enumerate() {
            let view_key = PrivateKey::from_hex(&entry.view_key_hex)
                .map_err(|_| anyhow!("Invalid view key hex for pool entry {}", entry.index))?;
            let spend_key = PrivateKey::from_hex(&entry.spend_key_hex)
                .map_err(|_| anyhow!("Invalid spend key hex for pool entry {}", entry.index))?;
            let spend_wallet = SpendWallet::new(spend_key, view_key, None);
            let wallet_type = WalletType::SpendWallet(spend_wallet);
            let km = KeyManager::new(wallet_type)
                .map_err(|e| anyhow!("KeyManager failed for pool_{}: {}", entry.index, e))?;
            key_managers.push(km);
            if (i + 1) % 250 == 0 {
                println!("  [drain]   ...{}/{} key managers built", i + 1, entries.len());
            }
        }
        println!("  [drain] All {} key managers ready", key_managers.len());

        // Create ONE scanner with ALL key managers — fetches blocks once,
        // checks all keys against each output using rayon
        println!("  [drain] Connecting to base node for bulk scan...");
        let mut bulk_scanner = HttpBlockchainScanner::new(
            base_node_url.to_string(),
            key_managers.clone(),
            0, // auto thread count
        )
        .await
        .map_err(|e| anyhow!("Failed to create bulk scanner: {}", e))?;
        println!("  [drain] Connected. Starting block scan...");

        let mut current_config = ScanConfig::default()
            .with_start_height(start_height)
            .with_batch_size(SCAN_BATCH_SIZE);

        let mut total_outputs = 0usize;
        let mut batch_num = 0u64;
        let scan_start = std::time::Instant::now();
        loop {
            batch_num += 1;
            println!(
                "  [drain] Fetching block batch {}...",
                batch_num
            );
            let (blocks, more_blocks) = bulk_scanner
                .scan_blocks(&current_config)
                .await
                .map_err(|e| anyhow!("Bulk scan failed: {}", e))?;

            if blocks.is_empty() {
                println!("  [drain] No more blocks to scan.");
                break;
            }

            let last_height = blocks.last().map(|b| b.height).unwrap_or(0);
            let _last_hash = blocks.last().map(|b| b.block_hash).unwrap_or_default();
            println!(
                "  [drain] Processing {} blocks (heights {}-{})...",
                blocks.len(),
                blocks.first().map(|b| b.height).unwrap_or(0),
                last_height,
            );

            for block in &blocks {
                for (output_hash, wallet_output, key_manager_index) in &block.wallet_outputs {
                    let entry = &entries[*key_manager_index];
                    let account_name = format!("pool_{}", entry.index);

                    let accounts = db::get_accounts(&conn, Some(&account_name))?;
                    let account = match accounts.first() {
                        Some(a) => a,
                        None => continue,
                    };

                    let view_key = key_managers[*key_manager_index].get_private_view_key();
                    let payment_reference =
                        generate_payment_reference(&block.block_hash, output_hash);

                    match db::insert_output(
                        &conn,
                        account.id,
                        &view_key,
                        output_hash.to_vec(),
                        wallet_output,
                        block.height,
                        &block.block_hash,
                        block.mined_timestamp,
                        None,
                        None,
                        payment_reference,
                    ) {
                        Ok(output_id) => {
                            // Immediately confirm — these outputs are long confirmed on chain
                            let _ = db::mark_output_confirmed(
                                &conn,
                                output_hash,
                                block.height,
                                block.block_hash.as_slice(),
                            );

                            // Create balance_change record so get_balance() sees the credit
                            let effective_date = DateTime::<Utc>::from_timestamp(
                                block.mined_timestamp as i64, 0
                            ).unwrap_or_else(Utc::now).naive_utc();

                            let balance_change = BalanceChange {
                                account_id: account.id,
                                caused_by_output_id: Some(output_id),
                                caused_by_input_id: None,
                                description: "Output found in drain scan".to_string(),
                                balance_credit: wallet_output.value(),
                                balance_debit: 0.into(),
                                effective_date,
                                effective_height: block.height,
                                claimed_recipient_address: None,
                                claimed_sender_address: None,
                                memo_parsed: None,
                                memo_hex: None,
                                claimed_fee: None,
                                claimed_amount: None,
                                is_reversal: false,
                                reversal_of_balance_change_id: None,
                                is_reversed: false,
                            };
                            if let Err(e) = db::insert_balance_change(&conn, &balance_change) {
                                warn!(
                                    "[drain] Failed to insert balance_change for pool_{}: {}",
                                    entry.index, e
                                );
                            }

                            total_outputs += 1;
                            println!(
                                "  [drain] Found output for pool_{}: {} µT at height {}",
                                entry.index,
                                wallet_output.value(),
                                block.height,
                            );
                        }
                        Err(e) => {
                            if !e.to_string().contains("UNIQUE constraint") {
                                warn!(
                                    "[drain] Failed to insert output for pool_{}: {}",
                                    entry.index, e
                                );
                            }
                        }
                    }
                }
            }

            let elapsed = scan_start.elapsed().as_secs();
            println!(
                "  [drain] Scanned to height {}/{}, {} outputs found ({}s elapsed)",
                last_height, tip_height, total_outputs, elapsed
            );

            if !more_blocks {
                break;
            }

            current_config = current_config.with_start_height(last_height + 1);
        }

        // Record scanned tip for all pool accounts so UTXO selector can find outputs
        let final_tip = wallet_client.get_tip_info().await?;
        let final_tip_height = final_tip
            .metadata
            .as_ref()
            .map(|m| m.best_block_height())
            .unwrap_or(tip_height);
        let final_tip_hash = final_tip
            .metadata
            .as_ref()
            .map(|m| m.best_block_hash().clone())
            .unwrap_or_default();

        println!("  [drain] Recording scanned tip at height {} for all pool accounts...", final_tip_height);
        for entry in entries.iter() {
            let account_name = format!("pool_{}", entry.index);
            if let Ok(accounts) = db::get_accounts(&conn, Some(&account_name)) {
                if let Some(account) = accounts.first() {
                    let _ = db::insert_scanned_tip_block(
                        &conn,
                        account.id,
                        final_tip_height as i64,
                        final_tip_hash.as_slice(),
                    );
                }
            }
        }

        let total_elapsed = scan_start.elapsed().as_secs();
        println!(
            "  [drain] Bulk scan complete in {}s: {} outputs found across {} pool accounts",
            total_elapsed, total_outputs, entries.len()
        );
        Ok(())
    }

    /// Drain funds from all pool accounts back to the destination address.
    /// Returns (total_drained, total_fees, accounts_drained).
    pub async fn drain_to(
        pool_db: &SqlitePool,
        _db_path: &Path,
        base_node_url: &str,
        destination: &str,
        entries: &[AddressPoolEntry],
        network: Network,
        confirmations: u64,
    ) -> anyhow::Result<(u64, u64, usize)> {
        let dest_address = TariAddress::from_base58(destination)
            .map_err(|e| anyhow!("Invalid destination address: {}", e))?;

        let conn = pool_db.get()?;
        let mut total_drained: u64 = 0;
        let mut total_fees: u64 = 0;
        let mut accounts_drained: usize = 0;
        let total = entries.len();

        for (i, entry) in entries.iter().enumerate() {
            let account_name = format!("pool_{}", entry.index);

            if (i + 1) % 100 == 0 || i == 0 {
                println!("  [drain] Checking balances {}/{}...", i + 1, total);
            }

            let accounts = db::get_accounts(&conn, Some(&account_name))?;
            let account = match accounts.first() {
                Some(a) => a,
                None => continue,
            };

            let balance = db::get_balance(&conn, account.id)?;
            if balance.available.0 == 0 {
                continue;
            }

            println!(
                "  [drain] pool_{}: available={} µT, sweeping to {}...",
                entry.index,
                balance.available.0,
                &destination[..20]
            );

            let fee_reserve: u64 = 10_000;
            let send_amount = balance.available.0.saturating_sub(fee_reserve);
            if send_amount == 0 {
                println!("  [drain] pool_{}: balance too low to sweep", entry.index);
                continue;
            }

            let mut sender = TransactionSender::new(
                pool_db.clone(),
                account_name.clone(),
                String::new(),
                network,
                confirmations,
            )?;

            let recipient = Recipient {
                address: dest_address.clone(),
                amount: MicroMinotari(send_amount),
                payment_id: None,
            };

            let idempotency_key = format!("drain_pool_{}", entry.index);

            let unsigned_tx = match sender.start_new_transaction(
                idempotency_key,
                recipient,
                SECONDS_TO_LOCK_UTXO,
            ) {
                Ok(tx) => tx,
                Err(e) => {
                    warn!("[drain] pool_{}: prepare failed: {}", entry.index, e);
                    continue;
                }
            };

            let fee = unsigned_tx.info.fee.0;

            let view_key = PrivateKey::from_hex(&entry.view_key_hex)
                .map_err(|_| anyhow!("Invalid view key hex"))?;
            let spend_key = PrivateKey::from_hex(&entry.spend_key_hex)
                .map_err(|_| anyhow!("Invalid spend key hex"))?;
            let spend_wallet = SpendWallet::new(spend_key, view_key, None);
            let wallet_type = WalletType::SpendWallet(spend_wallet);
            let key_manager = KeyManager::new(wallet_type)
                .map_err(|e| anyhow!("KeyManager creation failed: {}", e))?;

            let consensus_constants = ConsensusConstantsBuilder::new(network).build();

            let signed_tx = match sign_locked_transaction(
                &key_manager,
                consensus_constants,
                network,
                unsigned_tx,
            ) {
                Ok(tx) => tx,
                Err(e) => {
                    warn!("[drain] pool_{}: sign failed: {}", entry.index, e);
                    continue;
                }
            };

            match sender
                .finalize_transaction_and_broadcast(signed_tx, base_node_url.to_string())
                .await
            {
                Ok(_) => {
                    println!(
                        "  [drain] pool_{}: swept {} µT (fee: {} µT)",
                        entry.index, send_amount, fee
                    );
                    total_drained += send_amount;
                    total_fees += fee;
                    accounts_drained += 1;
                }
                Err(e) => {
                    warn!("[drain] pool_{}: broadcast failed: {}", entry.index, e);
                }
            }
        }

        Ok((total_drained, total_fees, accounts_drained))
    }
}
