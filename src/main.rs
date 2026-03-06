mod address_pool;
mod config;
mod driver;
mod fund_management;
mod load;
mod metrics;
mod scenarios;

use std::str::FromStr;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::Parser;
use log::{info, warn};
use tari_common::configuration::Network;
use tari_common_types::seeds::cipher_seed::CipherSeed;

use address_pool::AddressPool;
use config::{Cli, Command, ScenarioName, SetupState, WalletConfig};
use driver::new_wallet_driver::NewWalletDriver;
use driver::old_wallet_driver::OldWalletDriver;
use driver::WalletDriver;
use fund_management::{Consolidator, Drainer, Reconciler, Splitter};
use metrics::reporter::Reporter;
use metrics::system_monitor::SystemMonitor;
use metrics::MetricsCollector;
use scenarios::bidirectional::BidirectionalScenario;
use scenarios::fragmentation::FragmentationScenario;
use scenarios::inbound_flood::InboundFloodScenario;
use scenarios::lock_contention::LockContentionScenario;
use scenarios::pool_payout::PoolPayoutScenario;
use scenarios::Scenario;

fn create_drivers(config: &WalletConfig) -> anyhow::Result<(OldWalletDriver, NewWalletDriver)> {
    let network = Network::from_str(&config.network)
        .map_err(|e| anyhow::anyhow!("Invalid network '{}': {}", config.network, e))?;

    let seed_words = config.seed_words();
    if seed_words.is_empty() {
        bail!(
            "No seed words provided. Either pass --new-wallet-seed-words, \
             set TARI_SEED_WORDS env var, or run `setup` first."
        );
    }

    if !config.new_wallet_db.exists() {
        bail!(
            "Wallet database not found at {}. Run `setup` first or pass --new-wallet-db.",
            config.new_wallet_db.display()
        );
    }

    let old = OldWalletDriver::new(config.old_wallet_grpc.clone(), config.fee_per_gram);
    let new = NewWalletDriver::new(
        config.new_wallet_db.clone(),
        config.new_wallet_account.clone(),
        config.new_wallet_password.clone(),
        network,
        seed_words,
        config.resolved_base_node_url(),
        config.confirmations,
    )?;
    Ok((old, new))
}

fn get_scenario(name: &ScenarioName) -> Box<dyn Scenario> {
    match name {
        ScenarioName::PoolPayout => Box::new(PoolPayoutScenario),
        ScenarioName::InboundFlood => Box::new(InboundFloodScenario),
        ScenarioName::Bidirectional => Box::new(BidirectionalScenario),
        ScenarioName::Fragmentation => Box::new(FragmentationScenario),
        ScenarioName::LockContention => Box::new(LockContentionScenario),
    }
}

fn all_scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(PoolPayoutScenario),
        Box::new(InboundFloodScenario),
        Box::new(BidirectionalScenario),
        Box::new(FragmentationScenario),
        Box::new(LockContentionScenario),
    ]
}

/// Verify the wallet has enough balance for the scenario's budget.
async fn check_budget(wallet: &dyn WalletDriver, scenario: &dyn Scenario) -> anyhow::Result<()> {
    let balance = wallet.get_balance().await?;
    let budget = scenario.budget();

    if balance.available < budget {
        warn!(
            "[{}] Insufficient balance for {}: available={} < budget={}",
            wallet.name(),
            scenario.name(),
            balance.available,
            budget,
        );
    } else {
        info!(
            "[{}] Budget check passed for {}: available={} >= budget={}",
            wallet.name(),
            scenario.name(),
            balance.available,
            budget,
        );
    }
    Ok(())
}

fn init_address_pool(config: &WalletConfig) -> anyhow::Result<AddressPool> {
    if config.address_pool_file.exists() {
        info!(
            "Loading address pool from {}",
            config.address_pool_file.display()
        );
        let pool = AddressPool::load_from_file(&config.address_pool_file)?;
        info!("Loaded {} addresses from pool file", pool.len());
        Ok(pool)
    } else {
        let network = Network::from_str(&config.network)
            .map_err(|e| anyhow::anyhow!("Invalid network '{}': {}", config.network, e))?;
        info!(
            "Generating {} addresses for network {:?}",
            config.address_pool_size, network
        );
        let (pool, entries) = AddressPool::generate(config.address_pool_size, network)?;
        AddressPool::save_to_file(&entries, &config.address_pool_file)?;
        info!(
            "Saved {} addresses to {}",
            pool.len(),
            config.address_pool_file.display()
        );
        Ok(pool)
    }
}

/// Count UTXOs large enough to fund a send of `min_value` (amount + fee headroom).
fn count_usable_utxos(utxos: &[crate::driver::UtxoInfo], min_value: u64) -> u64 {
    utxos.iter().filter(|u| u.value >= min_value).count() as u64
}

async fn wait_for_pending_clear(
    wallet: &dyn WalletDriver,
    max_wait_secs: u64,
) -> anyhow::Result<()> {
    let poll_interval = tokio::time::Duration::from_secs(30);
    let start = tokio::time::Instant::now();
    let max_wait = tokio::time::Duration::from_secs(max_wait_secs);

    loop {
        wallet.sync_blockchain().await?;
        let balance = wallet.get_balance().await?;

        if balance.pending_incoming == 0 && balance.pending_outgoing == 0 {
            println!(
                "  [{}] No pending transactions. Available: {} tXTM",
                wallet.name(),
                balance.available / 1_000_000,
            );
            return Ok(());
        }

        println!(
            "  [{}] Pending in: {} tXTM, pending out: {} tXTM, available: {} tXTM ({}s elapsed)",
            wallet.name(),
            balance.pending_incoming / 1_000_000,
            balance.pending_outgoing / 1_000_000,
            balance.available / 1_000_000,
            start.elapsed().as_secs(),
        );

        if start.elapsed() > max_wait {
            println!(
                "  [{}] Warning: timed out waiting for pending to clear. Proceeding.",
                wallet.name(),
            );
            return Ok(());
        }

        tokio::time::sleep(poll_interval).await;
    }
}

async fn ensure_utxos(
    wallet: &dyn WalletDriver,
    scenario: &dyn Scenario,
    config: &WalletConfig,
    collector: &MetricsCollector,
) -> anyhow::Result<()> {
    let required = scenario.required_utxos();
    if required == 0 {
        return Ok(());
    }

    let split_amount = scenario.split_amount();
    let poll_interval = tokio::time::Duration::from_secs(30);
    let max_wait = tokio::time::Duration::from_secs(900);

    // Wait for any pending transactions (incoming or outgoing) to clear first
    let balance = wallet.get_balance().await?;
    if balance.pending_incoming > 0 || balance.pending_outgoing > 0 {
        println!(
            "  [{}] Waiting for pending txs to clear (in: {} tXTM, out: {} tXTM)...",
            wallet.name(),
            balance.pending_incoming / 1_000_000,
            balance.pending_outgoing / 1_000_000,
        );
        wait_for_pending_clear(wallet, 600).await?;
    }

    // Sync and count usable UTXOs (large enough for the scenario's sends)
    wallet.sync_blockchain().await?;
    let utxos = wallet.list_utxos().await.unwrap_or_default();
    let mut have = count_usable_utxos(&utxos, split_amount);

    if have >= required {
        println!(
            "  [{}] UTXOs: {} usable >= {} tXTM (need {}), ready.",
            wallet.name(),
            have,
            split_amount / 1_000_000,
            required,
        );
        return Ok(());
    }

    let needed = required - have;
    println!(
        "  [{}] UTXOs: {} usable (need {}). Splitting {} more at {} tXTM each...",
        wallet.name(),
        have,
        required,
        needed,
        split_amount / 1_000_000,
    );

    // Retry split with backoff if funds are pending/locked
    let mut split_done = false;
    let split_start = tokio::time::Instant::now();
    while !split_done {
        // Check available balance is sufficient before attempting split
        // Each split UTXO costs split_amount, plus ~5000 fee per UTXO as headroom
        let needed_now = required.saturating_sub(have);
        let required_balance = needed_now * (split_amount + 5_000);
        let balance = wallet.get_balance().await?;

        if balance.available < required_balance {
            if split_start.elapsed() > max_wait {
                bail!(
                    "[{}] Timed out waiting for sufficient available balance. \
                     Need {} tXTM, have {} tXTM available ({} tXTM pending out, {} tXTM pending in)",
                    wallet.name(),
                    required_balance / 1_000_000,
                    balance.available / 1_000_000,
                    balance.pending_outgoing / 1_000_000,
                    balance.pending_incoming / 1_000_000,
                );
            }
            println!(
                "  [{}] Available: {} tXTM, need {} tXTM for split. \
                 Pending out: {} tXTM, pending in: {} tXTM. Waiting 30s... ({}s elapsed)",
                wallet.name(),
                balance.available / 1_000_000,
                required_balance / 1_000_000,
                balance.pending_outgoing / 1_000_000,
                balance.pending_incoming / 1_000_000,
                split_start.elapsed().as_secs(),
            );
            tokio::time::sleep(poll_interval).await;
            wallet.sync_blockchain().await?;

            // Recheck UTXOs — maybe enough appeared from confirmations
            let utxos = wallet.list_utxos().await.unwrap_or_default();
            have = count_usable_utxos(&utxos, split_amount);
            if have >= required {
                println!(
                    "  [{}] UTXOs: {} usable (need {}), ready (no split needed).",
                    wallet.name(),
                    have,
                    required,
                );
                return Ok(());
            }
            continue;
        }

        match Splitter::split(
            wallet,
            split_amount,
            needed_now,
            200,
            config.confirmation_wait_secs(),
            collector,
        )
        .await
        {
            Ok(()) => {
                split_done = true;
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("FundsPending") || msg.contains("funds") {
                    if split_start.elapsed() > max_wait {
                        return Err(e.context("Timed out waiting for funds to settle before split"));
                    }
                    println!(
                        "  [{}] Funds still pending, waiting 30s before retrying split... ({}s elapsed)",
                        wallet.name(),
                        split_start.elapsed().as_secs(),
                    );
                    tokio::time::sleep(poll_interval).await;
                    wallet.sync_blockchain().await?;

                    // Recheck — maybe UTXOs appeared from prior activity
                    let utxos = wallet.list_utxos().await.unwrap_or_default();
                    have = count_usable_utxos(&utxos, split_amount);
                    if have >= required {
                        println!(
                            "  [{}] UTXOs: {} usable (need {}), ready (no split needed).",
                            wallet.name(),
                            have,
                            required,
                        );
                        return Ok(());
                    }
                } else {
                    return Err(e);
                }
            }
        }
    }

    // Splitter already waits for confirmations after each round,
    // so just verify we have enough usable UTXOs
    let utxos = wallet.list_utxos().await.unwrap_or_default();
    let final_have = count_usable_utxos(&utxos, split_amount);
    if final_have >= required {
        println!(
            "  [{}] UTXOs ready: {} usable (need {}).",
            wallet.name(),
            final_have,
            required,
        );
    } else {
        println!(
            "  [{}] Warning: only {} usable UTXOs (need {}). Some sends may fail.",
            wallet.name(),
            final_have,
            required,
        );
    }

    Ok(())
}

async fn run_scenario_for_wallet(
    scenario: &dyn Scenario,
    wallet: &dyn WalletDriver,
    address_pool: &AddressPool,
    config: &WalletConfig,
    collector: &MetricsCollector,
) -> anyhow::Result<()> {
    info!("=== Running {} on {} ===", scenario.name(), wallet.name());

    // Sync before checking budget so balance is accurate
    wallet.sync_blockchain().await?;

    // Ensure enough UTXOs exist before starting
    ensure_utxos(wallet, scenario, config, collector).await?;

    check_budget(wallet, scenario).await?;
    scenario
        .run(wallet, address_pool, config, collector)
        .await?;

    // Sync after scenario to pick up any self-sends or mined outputs
    wallet.sync_blockchain().await?;

    info!("=== {} complete on {} ===", scenario.name(), wallet.name());
    Ok(())
}

async fn run_all_scenarios_for_wallet(
    wallet: &dyn WalletDriver,
    initial_balance: u64,
    address_pool: &AddressPool,
    config: &WalletConfig,
    collector: &MetricsCollector,
) -> anyhow::Result<()> {
    let scenarios = all_scenarios();
    let fee_tolerance = 1_000_000; // 1 tXTM

    println!("\n========================================");
    println!("  {}: Running all 5 scenarios", wallet.name().to_uppercase());
    println!("  1. pool-payout     (~5 min)");
    println!("  2. inbound-flood   (~5 min)");
    println!("  3. bidirectional   (~31 min)");
    println!("  4. fragmentation   (~10 min)");
    println!("  5. lock-contention (~2 min)");
    println!("  Estimated total: ~55 min");
    println!("========================================\n");

    for (i, scenario) in scenarios.iter().enumerate() {
        println!(
            "--- {}: scenario {}/{} ---",
            wallet.name(),
            i + 1,
            scenarios.len()
        );
        run_scenario_for_wallet(scenario.as_ref(), wallet, address_pool, config, collector)
            .await?;

        info!(
            "Running inter-scenario checkpoint for {}...",
            wallet.name()
        );
        Reconciler::checkpoint(
            wallet,
            initial_balance,
            collector,
            config.confirmation_wait_secs(),
            fee_tolerance,
        )
        .await?;
    }

    // Final balance reconciliation for this wallet
    info!("=== Final Balance Reconciliation for {} ===", wallet.name());
    Reconciler::verify_balance(wallet, initial_balance, collector, fee_tolerance).await?;

    println!(
        "\n  {} complete. All 5 scenarios finished.\n",
        wallet.name().to_uppercase()
    );
    Ok(())
}

async fn run_baseline(
    old: &OldWalletDriver,
    new: &NewWalletDriver,
    address_pool: &AddressPool,
    _config: &WalletConfig,
    collector: &MetricsCollector,
) -> anyhow::Result<(u64, u64)> {
    info!("=== Syncing new wallet before baseline ===");
    new.sync().await?;
    info!("=== Phase 3: Baseline Measurements ===");

    // Check connectivity and initial balances
    let old_balance = old
        .get_balance()
        .await
        .context("Old wallet balance check")?;
    let new_balance = new
        .get_balance()
        .await
        .context("New wallet balance check")?;

    info!(
        "Old wallet balance: available={}, pending_in={}, locked={}",
        old_balance.available, old_balance.pending_incoming, old_balance.locked
    );
    info!(
        "New wallet balance: available={}, pending_in={}, locked={}",
        new_balance.available, new_balance.pending_incoming, new_balance.locked
    );

    // Get addresses
    let old_addr = old.get_address().await.context("Old wallet address")?;
    let new_addr = new.get_address().await.context("New wallet address")?;
    info!("Old wallet address: {}", old_addr);
    info!("New wallet address: {}", new_addr);

    // UTXO counts
    match old.list_utxos().await {
        Ok(utxos) => info!("Old wallet UTXO count: {}", utxos.len()),
        Err(e) => warn!("Could not list old wallet UTXOs: {}", e),
    }
    match new.list_utxos().await {
        Ok(utxos) => info!("New wallet UTXO count: {}", utxos.len()),
        Err(e) => warn!("Could not list new wallet UTXOs: {}", e),
    }

    // Single warm-up transaction from each wallet to an address pool sink
    {
        let warmup_addr = address_pool.next_address();
        info!("Warm-up: old wallet -> address pool");
        let result = old.send_transaction(warmup_addr, 1_000_000).await?; // 1 tXTM
        info!(
            "Old wallet warm-up: accepted={}, tx={}, fee={:?}",
            result.accepted, result.tx_id, result.fee
        );
        if let Some(fee) = result.fee {
            collector.record_transaction(metrics::recorder::TransactionRecord {
                wallet: "old_wallet".to_string(),
                scenario: "baseline".to_string(),
                tx_id: result.tx_id,
                start_time: chrono::Utc::now(),
                end_time: chrono::Utc::now(),
                duration_ms: 0,
                accepted: result.accepted,
                error: result.error,
                amount: 1_000_000,
                fee: Some(fee),
                tx_type: "warmup".to_string(),
            });
        }
    }

    {
        let warmup_addr = address_pool.next_address();
        info!("Warm-up: new wallet -> address pool");
        let result = new.send_transaction(warmup_addr, 1_000_000).await?; // 1 tXTM
        info!(
            "New wallet warm-up: accepted={}, tx={}, fee={:?}",
            result.accepted, result.tx_id, result.fee
        );
        if let Some(fee) = result.fee {
            collector.record_transaction(metrics::recorder::TransactionRecord {
                wallet: "new_wallet".to_string(),
                scenario: "baseline".to_string(),
                tx_id: result.tx_id,
                start_time: chrono::Utc::now(),
                end_time: chrono::Utc::now(),
                duration_ms: 0,
                accepted: result.accepted,
                error: result.error,
                amount: 1_000_000,
                fee: Some(fee),
                tx_type: "warmup".to_string(),
            });
        }
    }

    info!("Baseline measurements complete");
    Ok((old_balance.available, new_balance.available))
}

async fn run_setup(setup_config: &config::SetupConfig) -> anyhow::Result<()> {
    let network = Network::from_str(&setup_config.network)
        .map_err(|e| anyhow::anyhow!("Invalid network '{}': {}", setup_config.network, e))?;

    // Create data directory
    std::fs::create_dir_all(&setup_config.data_dir)?;
    info!(
        "Created data directory: {}",
        setup_config.data_dir.display()
    );

    // --- Step 1: Create new wallet (or restore from existing seed words) ---
    let db_path = setup_config.data_dir.join("new_wallet.sqlite");
    if db_path.exists() {
        info!("New wallet already exists at {}", db_path.display());
    } else {
        // Check if we have seed words from a previous setup to restore
        let cipher_seed = if let Some(state) = SetupState::load() {
            if !state.new_wallet_seed_words.is_empty() {
                info!("Restoring wallet from existing seed words in setup.json");
                let seed_words_parsed =
                    tari_common_types::seeds::seed_words::SeedWords::from_str(
                        &state.new_wallet_seed_words,
                    )
                    .map_err(|e| anyhow::anyhow!("Invalid seed words in setup.json: {}", e))?;
                use tari_common_types::seeds::mnemonic::Mnemonic;
                CipherSeed::from_mnemonic(&seed_words_parsed, None)
                    .map_err(|e| anyhow::anyhow!("Failed to restore seed: {}", e))?
            } else {
                info!("Creating new wallet with fresh seed");
                CipherSeed::random()
            }
        } else {
            info!("Creating new wallet with fresh seed");
            CipherSeed::random()
        };

        minotari::utils::init_wallet::init_with_seed_words(
            cipher_seed,
            "",
            &db_path,
            Some("default"),
        )?;
        info!("New wallet created successfully");
    }

    // Open the new wallet and get its address + seed words
    let db_pool = minotari::init_db(db_path.clone())?;
    let conn = db_pool.get()?;
    let accounts = minotari::get_accounts(&conn, Some("default"))?;
    let account = accounts
        .first()
        .ok_or_else(|| anyhow::anyhow!("Account 'default' not found in new wallet"))?;

    let new_address = account.get_address(network, "")?;
    let new_address_str = new_address.to_base58();

    let seed_words = account
        .get_seed_words("")?
        .ok_or_else(|| anyhow::anyhow!("Could not retrieve seed words from wallet"))?;
    let seed_words_str = (0..seed_words.len())
        .map(|i| seed_words.get_word(i).unwrap().clone())
        .collect::<Vec<_>>()
        .join(" ");

    // --- Step 2: Connect to old wallet and get its address ---
    info!(
        "Connecting to old wallet at {}...",
        setup_config.old_wallet_grpc
    );
    let old_wallet = OldWalletDriver::new(setup_config.old_wallet_grpc.clone(), 5);
    let old_address = old_wallet.get_address().await.context(
        "Failed to connect to old wallet. Is minotari_console_wallet running with --grpc-enabled?",
    )?;

    // --- Step 3: Resolve base node URL ---
    let resolved_base_node_url = setup_config
        .base_node_url
        .clone()
        .unwrap_or_else(|| config::default_base_node_url(&setup_config.network));

    // --- Step 4: Display addresses ---
    println!("\n========================================");
    println!("  WALLET SETUP COMPLETE");
    println!("========================================\n");
    println!("OLD WALLET (minotari_console_wallet)");
    println!("  Address: {}", old_address);
    println!();
    println!("NEW WALLET (minotari-cli)");
    println!("  Address: {}", new_address_str);
    println!("  DB path: {}", db_path.display());
    println!();
    println!("SEED WORDS (back these up!):");
    println!("  {}", seed_words_str);
    println!();
    println!("BASE NODE RPC: {}", resolved_base_node_url);
    println!("NETWORK: {}", setup_config.network);
    println!();
    println!("========================================");
    println!(
        "  Fund both wallets with at least {} tXTM each",
        setup_config.min_balance_txm
    );
    println!("  Waiting for funding...");
    println!("========================================\n");

    // --- Step 5: Save setup state ---

    let state = SetupState {
        old_wallet_grpc: setup_config.old_wallet_grpc.clone(),
        new_wallet_db: db_path.to_string_lossy().to_string(),
        new_wallet_seed_words: seed_words_str,
        base_node_url: resolved_base_node_url.clone(),
        network: setup_config.network.clone(),
        old_wallet_address: old_address.clone(),
        new_wallet_address: new_address_str.clone(),
    };
    state.save()?;
    info!("Setup state saved to {}", SetupState::path().display());

    // --- Step 5: Sync + poll until both wallets are funded ---
    let min_balance_micro = setup_config.min_balance_txm * 1_000_000; // tXTM to MicroMinotari

    let new_wallet = NewWalletDriver::new(
        db_path,
        "default".to_string(),
        String::new(),
        network,
        state
            .new_wallet_seed_words
            .split_whitespace()
            .map(|s| s.to_string())
            .collect(),
        resolved_base_node_url.clone(),
        3,
    )?;

    // Initial sync — catches up from wallet birthday to chain tip
    println!("  Syncing new wallet with blockchain...");
    if let Err(e) = new_wallet.sync().await {
        warn!("Initial sync failed: {}", e);
    }

    loop {
        // Check balances (reads from local DB, no network call for new wallet)
        let old_ok = match old_wallet.get_balance().await {
            Ok(b) => {
                println!(
                    "  Old wallet: {} tXTM (need {})",
                    b.available / 1_000_000,
                    setup_config.min_balance_txm
                );
                b.available >= min_balance_micro
            }
            Err(e) => {
                println!("  Old wallet: error - {}", e);
                false
            }
        };

        let new_ok = match new_wallet.get_balance().await {
            Ok(b) => {
                println!(
                    "  New wallet: {} tXTM (need {})",
                    b.available / 1_000_000,
                    setup_config.min_balance_txm
                );
                b.available >= min_balance_micro
            }
            Err(e) => {
                println!("  New wallet: error - {}", e);
                false
            }
        };

        if old_ok && new_ok {
            println!("\n  Both wallets funded! Ready to run tests.\n");
            break;
        }

        // Wait 30s, then scan for new blocks before checking again
        println!("  Waiting 30s before next check...");
        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

        if let Err(e) = new_wallet.sync_to_tip().await {
            warn!("Scan failed: {}", e);
        }
    }

    // --- Step 6: Generate address pool ---
    let pool_path = setup_config.data_dir.join("address_pool.json");
    if pool_path.exists() {
        info!("Address pool already exists at {}", pool_path.display());
    } else {
        info!(
            "Generating {} addresses for address pool...",
            setup_config.address_pool_size
        );
        let (pool, entries) = AddressPool::generate(setup_config.address_pool_size, network)?;
        AddressPool::save_to_file(&entries, &pool_path)?;
        info!("Saved {} addresses to {}", pool.len(), pool_path.display());
    }

    println!("Setup complete! Run tests with:\n");
    println!("  cargo run -- run-all");
    println!();

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    env_logger::Builder::new()
        .filter_level(cli.log_level.parse().unwrap_or(log::LevelFilter::Info))
        .init();

    let collector = Arc::new(MetricsCollector::new());

    match cli.command {
        Command::Setup { setup_config } => {
            run_setup(&setup_config).await?;
        }

        Command::RunAll { wallet_config } => {
            let address_pool = init_address_pool(&wallet_config)?;
            let (old, new) = create_drivers(&wallet_config)?;
            let old = Arc::new(old);
            let new = Arc::new(new);

            // Start system monitors
            let (old_cancel_tx, old_cancel_rx) = tokio::sync::watch::channel(false);
            let (new_cancel_tx, new_cancel_rx) = tokio::sync::watch::channel(false);
            let old_monitor = SystemMonitor::start(
                30,
                collector.clone(),
                old.clone() as Arc<dyn WalletDriver>,
                old_cancel_rx,
            );
            let new_monitor = SystemMonitor::start(
                30,
                collector.clone(),
                new.clone() as Arc<dyn WalletDriver>,
                new_cancel_rx,
            );

            // Baseline — capture initial balances for reconciliation
            let (old_initial, new_initial) =
                run_baseline(&old, &new, &address_pool, &wallet_config, &collector).await?;

            // Run old wallet, write intermediate report
            run_all_scenarios_for_wallet(
                old.as_ref(),
                old_initial,
                &address_pool,
                &wallet_config,
                &collector,
            )
            .await?;
            Reporter::write_reports(&collector, &cli.output_dir.join("old_wallet"))?;
            Reporter::print_comparison(&collector);

            // Run new wallet, write intermediate report
            run_all_scenarios_for_wallet(
                new.as_ref(),
                new_initial,
                &address_pool,
                &wallet_config,
                &collector,
            )
            .await?;

            // Stop system monitors
            let _ = old_cancel_tx.send(true);
            let _ = new_cancel_tx.send(true);
            let _ = old_monitor.await;
            let _ = new_monitor.await;

            // Final combined reports
            Reporter::write_reports(&collector, &cli.output_dir)?;
            Reporter::print_comparison(&collector);
        }

        Command::RunOld { wallet_config } => {
            let address_pool = init_address_pool(&wallet_config)?;
            let (old, _new) = create_drivers(&wallet_config)?;
            let old = Arc::new(old);

            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            let monitor = SystemMonitor::start(
                30,
                collector.clone(),
                old.clone() as Arc<dyn WalletDriver>,
                cancel_rx,
            );

            old.sync_blockchain().await?;
            let initial_balance = old.get_balance().await?.available;

            run_all_scenarios_for_wallet(
                old.as_ref(),
                initial_balance,
                &address_pool,
                &wallet_config,
                &collector,
            )
            .await?;

            let _ = cancel_tx.send(true);
            let _ = monitor.await;

            Reporter::write_reports(&collector, &cli.output_dir)?;
            Reporter::print_comparison(&collector);
        }

        Command::RunNew { wallet_config } => {
            let address_pool = init_address_pool(&wallet_config)?;
            let (_old, new) = create_drivers(&wallet_config)?;
            let new = Arc::new(new);

            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            let monitor = SystemMonitor::start(
                30,
                collector.clone(),
                new.clone() as Arc<dyn WalletDriver>,
                cancel_rx,
            );

            info!("=== Syncing new wallet ===");
            new.sync().await?;
            let initial_balance = new.get_balance().await?.available;

            run_all_scenarios_for_wallet(
                new.as_ref(),
                initial_balance,
                &address_pool,
                &wallet_config,
                &collector,
            )
            .await?;

            let _ = cancel_tx.send(true);
            let _ = monitor.await;

            Reporter::write_reports(&collector, &cli.output_dir)?;
            Reporter::print_comparison(&collector);
        }

        Command::Run {
            scenario,
            wallet_config,
        } => {
            let address_pool = init_address_pool(&wallet_config)?;
            let (old, new) = create_drivers(&wallet_config)?;

            let scenario = get_scenario(&scenario);

            // Run on old wallet first, then new wallet (sequential)
            run_scenario_for_wallet(
                scenario.as_ref(),
                &old,
                &address_pool,
                &wallet_config,
                &collector,
            )
            .await?;

            run_scenario_for_wallet(
                scenario.as_ref(),
                &new,
                &address_pool,
                &wallet_config,
                &collector,
            )
            .await?;

            Reporter::write_reports(&collector, &cli.output_dir)?;
            Reporter::print_comparison(&collector);
        }

        Command::Baseline { wallet_config } => {
            let address_pool = init_address_pool(&wallet_config)?;
            let (old, new) = create_drivers(&wallet_config)?;
            run_baseline(&old, &new, &address_pool, &wallet_config, &collector).await?;
        }

        Command::Consolidate { wallet_config } => {
            let (old, new) = create_drivers(&wallet_config)?;

            info!("Consolidating old wallet...");
            Consolidator::consolidate(&old, wallet_config.confirmation_wait_secs()).await?;

            info!("Consolidating new wallet...");
            Consolidator::consolidate(&new, wallet_config.confirmation_wait_secs()).await?;

            // Final balance report
            let old_balance = old.get_balance().await?;
            let new_balance = new.get_balance().await?;
            info!(
                "Old wallet final balance: available={}",
                old_balance.available
            );
            info!(
                "New wallet final balance: available={}",
                new_balance.available
            );
            info!("Consolidation complete");
        }

        Command::Drain {
            wallet_config,
            drain_limit,
        } => {
            let network = Network::from_str(&wallet_config.network)
                .map_err(|e| anyhow::anyhow!("Invalid network: {}", e))?;
            let base_node_url = wallet_config.resolved_base_node_url();

            // Load pool entries (need full entries with private keys)
            if !wallet_config.address_pool_file.exists() {
                bail!(
                    "Address pool file not found at {}. Run setup first.",
                    wallet_config.address_pool_file.display()
                );
            }
            let pool_data = std::fs::read_to_string(&wallet_config.address_pool_file)?;
            let all_entries: Vec<crate::address_pool::AddressPoolEntry> =
                serde_json::from_str(&pool_data)?;

            // Limit entries if requested
            let entries: Vec<_> = if drain_limit > 0 && drain_limit < all_entries.len() {
                println!(
                    "  Limiting drain to first {} of {} pool accounts",
                    drain_limit,
                    all_entries.len()
                );
                all_entries.into_iter().take(drain_limit).collect()
            } else {
                all_entries
            };
            println!(
                "  Loaded {} pool entries from {}",
                entries.len(),
                wallet_config.address_pool_file.display()
            );

            // Open wallet DB
            let db_pool = minotari::init_db(wallet_config.new_wallet_db.clone())?;

            // Step 1: Register pool addresses as accounts
            println!("  Registering pool addresses as wallet accounts...");
            let registered = Drainer::register_pool_accounts(&db_pool, &entries)?;
            println!("  Registered {} new pool accounts", registered);

            // Step 2: Scan pool accounts to discover outputs
            println!(
                "  Scanning {} pool accounts for outputs...",
                entries.len()
            );
            println!("  (Each account creates an HTTP connection + scans ~30 blocks)");
            Drainer::scan_pool_accounts(
                &wallet_config.new_wallet_db,
                &base_node_url,
                &entries,
                wallet_config.confirmations,
            )
            .await?;

            // Step 3: Get destination address (the main new wallet)
            let conn = db_pool.get()?;
            let accounts = minotari::get_accounts(&conn, Some("default"))?;
            let main_account = accounts
                .first()
                .ok_or_else(|| anyhow::anyhow!("Main 'default' account not found"))?;
            let dest_address = main_account.get_address(network, "")?.to_base58();
            drop(conn);

            println!(
                "  Draining pool funds to main wallet: {}...",
                &dest_address[..20]
            );

            // Step 4: Drain all pool accounts
            let (total_drained, total_fees, count) = Drainer::drain_to(
                &db_pool,
                &wallet_config.new_wallet_db,
                &base_node_url,
                &dest_address,
                &entries,
                network,
                wallet_config.confirmations,
            )
            .await?;

            println!("\n========================================");
            println!("  DRAIN COMPLETE");
            println!("========================================");
            println!("  Accounts drained: {}", count);
            println!(
                "  Total recovered:  {} µT ({} tXTM)",
                total_drained,
                total_drained / 1_000_000
            );
            println!("  Total fees:       {} µT", total_fees);
            println!("========================================\n");
        }
    }

    Ok(())
}
