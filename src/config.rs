use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Returns the default base node RPC URL for a given network name.
pub fn default_base_node_url(network: &str) -> String {
    match network.to_lowercase().as_str() {
        "esmeralda" => "https://rpc.esmeralda.tari.com".to_string(),
        "nextnet" => "https://rpc.nextnet.tari.com".to_string(),
        "mainnet" => "https://rpc.tari.com".to_string(),
        _ => "https://rpc.tari.com".to_string(),
    }
}

#[derive(Parser)]
#[command(name = "wallet-performance")]
#[command(
    about = "Performance comparison: minotari_console_wallet (gRPC Transfer) vs minotari-cli (local UTXO selection + sign + broadcast)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Output directory for metrics CSV files
    #[arg(long, default_value = "results")]
    pub output_dir: PathBuf,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create wallets, display addresses, and wait for funding before running tests
    Setup {
        #[command(flatten)]
        setup_config: SetupConfig,
    },
    /// Run all scenarios sequentially on both wallets
    RunAll {
        #[command(flatten)]
        wallet_config: WalletConfig,
    },
    /// Run all scenarios on old wallet only (minotari_console_wallet via gRPC)
    RunOld {
        #[command(flatten)]
        wallet_config: WalletConfig,
    },
    /// Run all scenarios on new wallet only (minotari-cli)
    RunNew {
        #[command(flatten)]
        wallet_config: WalletConfig,
    },
    /// Run a specific scenario on both wallets
    Run {
        /// Scenario to run (pool-payout, inbound-flood, bidirectional, fragmentation, lock-contention)
        #[arg(value_enum)]
        scenario: ScenarioName,
        #[command(flatten)]
        wallet_config: WalletConfig,
    },
    /// Run baseline measurements only
    Baseline {
        #[command(flatten)]
        wallet_config: WalletConfig,
    },
    /// Consolidate all UTXOs back to single UTXO per wallet
    Consolidate {
        #[command(flatten)]
        wallet_config: WalletConfig,
    },
    /// Run pool/payment-processor scenarios on new wallet (batch 1-to-many transactions)
    RunPool {
        #[command(flatten)]
        wallet_config: WalletConfig,
    },
    /// Sync the new wallet to chain tip and print balance
    Sync {
        #[command(flatten)]
        wallet_config: WalletConfig,
    },
    /// Drain funds from address pool sink wallets back to the new wallet
    Drain {
        #[command(flatten)]
        wallet_config: WalletConfig,

        /// Maximum number of pool accounts to scan/drain (0 = all).
        /// Only accounts that received funds need draining.
        #[arg(long, default_value_t = 0)]
        drain_limit: usize,
    },
}

/// Configuration for the setup command (creates wallets automatically).
#[derive(Clone, Debug, clap::Args)]
pub struct SetupConfig {
    /// Path to the minotari_console_wallet binary
    #[arg(long, default_value = "minotari_console_wallet")]
    pub old_wallet_binary: PathBuf,

    /// Base directory for old wallet data
    #[arg(long, default_value = "data/old_wallet")]
    pub old_wallet_base_dir: PathBuf,

    /// gRPC port for managed old wallet process
    #[arg(long, default_value_t = 18143)]
    pub old_wallet_grpc_port: u16,

    /// Base node HTTP RPC URL for broadcasting transactions (new wallet).
    /// Defaults to the public RPC for the selected network.
    #[arg(long)]
    pub base_node_url: Option<String>,

    /// Network (mainnet, nextnet, esmeralda, localnet)
    #[arg(long, default_value = "esmeralda")]
    pub network: String,

    /// Directory for wallet data (created automatically)
    #[arg(long, default_value = "data")]
    pub data_dir: PathBuf,

    /// Minimum balance (in tXTM) required before proceeding
    #[arg(long, default_value_t = 100_000)]
    pub min_balance_txm: u64,

    /// Number of addresses to generate in the address pool
    #[arg(long, default_value_t = 1000)]
    pub address_pool_size: usize,
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum ScenarioName {
    PoolPayout,
    InboundFlood,
    Bidirectional,
    Fragmentation,
    LockContention,
}

/// Persisted setup state written by the `setup` command.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SetupState {
    pub old_wallet_grpc: String,
    #[serde(default)]
    pub old_wallet_binary: String,
    pub new_wallet_db: String,
    pub new_wallet_seed_words: String,
    pub base_node_url: String,
    pub network: String,
    pub old_wallet_address: String,
    pub new_wallet_address: String,
}

impl SetupState {
    pub fn path() -> PathBuf {
        PathBuf::from("data/setup.json")
    }

    pub fn load() -> Option<Self> {
        let path = Self::path();
        let data = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&data).ok()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }
}

#[derive(Clone, Debug, clap::Args)]
pub struct WalletConfig {
    // === Old wallet (minotari_console_wallet via gRPC) ===
    /// Old wallet gRPC address (uses single-call Transfer RPC)
    #[arg(long, default_value = "http://127.0.0.1:18143")]
    pub old_wallet_grpc: String,

    /// Path to the minotari_console_wallet binary (for process management)
    #[arg(long, default_value = "minotari_console_wallet")]
    pub old_wallet_binary: PathBuf,

    /// Base directory for old wallet data (deleted and recreated for fresh scans)
    #[arg(long, default_value = "data/old_wallet")]
    pub old_wallet_base_dir: PathBuf,

    /// gRPC port for managed old wallet process
    #[arg(long, default_value_t = 18143)]
    pub old_wallet_grpc_port: u16,

    /// Fee per gram in MicroMinotari (old wallet)
    #[arg(long, default_value_t = 5)]
    pub fee_per_gram: u64,

    // === New wallet (minotari-cli, local DB + signing) ===
    /// Path to the minotari-cli wallet SQLite database (default: data/new_wallet.sqlite)
    #[arg(long, default_value = "data/new_wallet.sqlite")]
    pub new_wallet_db: PathBuf,

    /// Account name in the minotari-cli wallet
    #[arg(long, default_value = "default")]
    pub new_wallet_account: String,

    /// Password for the minotari-cli wallet account
    #[arg(long, env = "TARI_WALLET_PASSWORD", default_value = "")]
    pub new_wallet_password: String,

    /// Seed words for the new wallet (space-separated mnemonic). Auto-loaded from data/setup.json if not provided.
    #[arg(long, env = "TARI_SEED_WORDS")]
    pub new_wallet_seed_words: Option<String>,

    /// Base node HTTP RPC URL for broadcasting transactions (new wallet).
    /// Defaults to the public RPC for the selected network.
    #[arg(long)]
    pub base_node_url: Option<String>,

    // === Shared settings ===
    /// Network (mainnet, nextnet, esmeralda, localnet)
    #[arg(long, default_value = "esmeralda")]
    pub network: String,

    /// Block time in seconds
    #[arg(long, default_value_t = 120)]
    pub block_time_secs: u64,

    /// Number of confirmations to wait for
    #[arg(long, default_value_t = 3)]
    pub confirmations: u64,

    /// Number of addresses to generate in the address pool
    #[arg(long, default_value_t = 1000)]
    pub address_pool_size: usize,

    /// Path to the address pool JSON file (loaded if exists, generated otherwise)
    #[arg(long, default_value = "data/address_pool.json")]
    pub address_pool_file: PathBuf,
}

impl WalletConfig {
    /// Resolved base node URL: explicit flag > setup.json > network default.
    pub fn resolved_base_node_url(&self) -> String {
        if let Some(ref url) = self.base_node_url {
            return url.clone();
        }
        if let Some(state) = SetupState::load() {
            return state.base_node_url;
        }
        default_base_node_url(&self.network)
    }

    /// Confirmation wait time = block_time * confirmations
    pub fn confirmation_wait_secs(&self) -> u64 {
        self.block_time_secs * self.confirmations
    }

    /// Get seed words, falling back to setup.json if not provided via CLI/env.
    pub fn seed_words(&self) -> Vec<String> {
        if let Some(ref words) = self.new_wallet_seed_words {
            return words.split_whitespace().map(|s| s.to_string()).collect();
        }

        // Fall back to setup.json
        if let Some(state) = SetupState::load() {
            return state
                .new_wallet_seed_words
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
        }

        Vec::new()
    }
}
