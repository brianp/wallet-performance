# Wallet Performance Comparison Test

Performance test harness comparing two transaction flows:

- **Old wallet** — `minotari_console_wallet` spawned and managed as a child process; communicates via gRPC `Transfer` RPC (wallet handles signing internally)
- **New wallet** — `minotari-cli` library: local UTXO selection → `sign_locked_transaction` → broadcast via HTTP RPC

Both wallets share the same seed words. The harness spawns a fresh `minotari_console_wallet` process for the old wallet, deletes its data directory between runs for clean scan measurements, and manages its lifecycle (including SIGTERM on drop).

Both exercise fundamentally different code paths. The new flow is the offline signing model used by Tari Universe / minotari-cli.

## Overview

This tool hammers both transaction paths with identical workloads and compares:

- **Scan time** — Time to sync/scan the blockchain from scratch (both wallets start fresh each run)
- **Throughput** — Transactions per second (TPS) with constant, ramp, burst, and Poisson load patterns
- **Latency** — p50, p95, p99 response times per scenario
- **Error rates** — Percentage of rejected/failed transactions, UTXO lock contention
- **Fee efficiency** — Cumulative fee tracking per wallet, per scenario
- **UTXO handling** — Splitting, fragmentation, input aggregation performance
- **Concurrency** — Lock contention under rapid sequential sends (10/25/50 batches)
- **Batch sends** — Payment processor pattern: 1-to-many transactions in a single tx (new wallet only)
- **System resources** — CPU and memory monitoring throughout test runs
- **Balance reconciliation** — Automated inter-scenario and final balance verification

## Prerequisites

1. **`minotari_console_wallet`** binary in `$PATH` (or specify with `--old-wallet-binary`)
2. Both wallets funded with testnet tXTM (minimum 100,000 tXTM each recommended)
3. A **base node HTTP RPC** endpoint for broadcasting new wallet transactions (defaults to the public RPC for the selected network)

## Building

```bash
cd wallet-performance
cargo build --release
```

## Usage

### 1. Setup (automated)

The `setup` command creates both wallets, displays addresses, and waits for funding:

```bash
cargo run -- setup \
  --network esmeralda
```

This will:
1. Create a `data/` directory with a new minotari-cli wallet (auto-generated seed words)
2. Spawn a `minotari_console_wallet` process using the same seed words
3. Display both wallet addresses and the generated seed words
4. Poll both wallets until they each have sufficient balance (default: 100,000 tXTM)
5. Generate the address pool (1000 addresses)
6. Save all configuration to `data/setup.json` so subsequent commands need no extra flags

After setup, fund both displayed addresses and wait for the tool to detect the balances.

### 2. Run all scenarios

```bash
cargo run -- run-all
```

If you ran `setup` first, no extra flags are needed — configuration is loaded from `data/setup.json`. You can override any setting:

```bash
cargo run -- run-all \
  --old-wallet-grpc http://127.0.0.1:18143 \
  --new-wallet-db data/new_wallet.sqlite \
  --new-wallet-seed-words "word1 word2 ... word24" \
  --network esmeralda \
  --output-dir results
```

This will:
1. **Old wallet**: delete data dir → spawn fresh `minotari_console_wallet` → measure scan time → run all scenarios → write reports → kill process
2. **New wallet**: delete wallet DB → recreate from seed → measure scan time → run all scenarios → write reports
3. Background system monitors sample CPU/memory every 30s throughout
4. Inter-scenario balance reconciliation checkpoints between each scenario
5. Write combined CSV reports and print comparison summary

### 3. Run a single wallet

```bash
cargo run -- run-old   # Old wallet only (fresh scan + all scenarios)
cargo run -- run-new   # New wallet only (fresh scan + all scenarios)
```

### 4. Run a single scenario

```bash
cargo run -- run pool-payout
```

Runs the specified scenario on both wallets (fresh scan for each).

Available scenarios:

| Scenario | Description | Budget |
|----------|-------------|--------|
| `pool-payout` | Split UTXOs → constant 5 TPS for 2 min → burst 50 txs | 40,000 tXTM |
| `inbound-flood` | Poisson-rate sends + monitor inbound detection for 10 min | 20,000 tXTM |
| `bidirectional` | Ramp send rate from 1/min to 5/min over 30 min | 20,000 tXTM |
| `fragmentation` | Cascade split into 500 UTXOs → test aggregation sends | 10,000 tXTM |
| `lock-contention` | 10/25/50 rapid sequential sends measuring contention | 10,000 tXTM |

### 5. Run pool/payment processor mode

```bash
cargo run -- run-pool
```

Runs all scenarios on the new wallet only, with `pool_payout` using batch 1-to-many sends (50 recipients per transaction) instead of individual sends. This exercises the payment processor pattern where a single transaction pays multiple recipients.

### 6. Run baseline only

```bash
cargo run -- baseline
```

Spawns both wallets fresh, measures scan times, checks connectivity, records initial balances, and sends a single warm-up transaction from each wallet.

### 7. Sync new wallet

```bash
cargo run -- sync
```

Syncs the new wallet to chain tip and prints the current balance. Does not spawn a fresh wallet — uses the existing DB.

### 8. Consolidate UTXOs after testing

```bash
cargo run -- consolidate
```

Sweeps all UTXOs to a single output per wallet, polls for mining confirmation, and reports final balances.

### 9. Drain address pool funds

```bash
cargo run -- drain
```

Registers pool addresses as wallet accounts, scans them for outputs via HTTP, then sweeps everything back to the main new wallet. Use `--drain-limit N` to limit scanning to the first N pool accounts.

## Configuration Options

| Flag | Default | Description |
|------|---------|-------------|
| `--old-wallet-grpc` | `http://127.0.0.1:18143` | Old wallet gRPC endpoint |
| `--old-wallet-binary` | `minotari_console_wallet` | Path to the console wallet binary |
| `--old-wallet-base-dir` | `data/old_wallet` | Base directory for old wallet data (deleted for fresh scans) |
| `--old-wallet-grpc-port` | `18143` | gRPC port for managed old wallet process |
| `--fee-per-gram` | `5` | Fee per gram in MicroMinotari (old wallet) |
| `--new-wallet-db` | `data/new_wallet.sqlite` | Path to minotari-cli SQLite database |
| `--new-wallet-account` | `default` | Account name in minotari-cli wallet |
| `--new-wallet-password` | `""` (env: `TARI_WALLET_PASSWORD`) | Password for minotari-cli account |
| `--new-wallet-seed-words` | — (env: `TARI_SEED_WORDS`) | Space-separated 24-word mnemonic |
| `--base-node-url` | network-dependent (e.g. `https://rpc.esmeralda.tari.com`) | Base node HTTP RPC for broadcasting (new wallet) |
| `--network` | `esmeralda` | Network (mainnet, nextnet, esmeralda, localnet) |
| `--block-time-secs` | `120` | Block time for confirmation calculations |
| `--confirmations` | `3` | Confirmations to wait between phases |
| `--address-pool-size` | `1000` | Number of receive-only sink addresses to generate |
| `--address-pool-file` | `data/address_pool.json` | Path to address pool JSON (loaded if exists, generated otherwise) |
| `--output-dir` | `results` | Directory for CSV output files |
| `--log-level` | `info` | Log verbosity (error/warn/info/debug/trace) |

## Output

Results are written to the output directory as CSV files:

- **`transactions.csv`** — Per-transaction timing data: wallet, scenario, duration_ms, accepted, error, amount, fee, tx_type
- **`scans.csv`** — Blockchain scan measurements: wallet, scan_type, duration_ms, start/end height, blocks scanned
- **`system_snapshots.csv`** — Periodic CPU/memory/balance samples per wallet
- **`summary.csv`** — Aggregated stats per scenario per wallet: TPS, p50/p95/p99, error rate, total fees

Subcommands write to subdirectories: `run-all` writes per-wallet reports to `results/old_wallet/` and `results/new_wallet/` plus combined reports to `results/`. `run-pool` writes to `results/pool/`.

A comparison summary is printed to the console after each run.

## Transaction Flow Comparison

### Old Wallet (managed process + gRPC Transfer)

```
Harness → spawn minotari_console_wallet (fresh data dir)
        → wait for gRPC ready + blockchain scan complete
        → Transfer gRPC → wallet signs internally → broadcast → done
        → SIGTERM on drop
```

The harness manages the full process lifecycle. One gRPC call handles UTXO selection, signing, and broadcast.

### New Wallet (minotari-cli library)

```
Harness → init wallet DB from seed words
        → sync_to_tip() (scan blockchain)
        → TransactionSender.start_new_transaction() → unsigned tx (UTXO selection + locking)
          ↓
        → sign_locked_transaction() → signed tx (local key derivation from seed words)
          ↓
        → TransactionSender.finalize_transaction_and_broadcast() → HTTP RPC to base node → done
```

Three distinct phases using the `minotari` crate directly:
1. **Prepare** — UTXO selection from SQLite, fee estimation, transaction construction, UTXO locking
2. **Sign** — Key derivation from seed words, transaction signing (all in-process, no external binary)
3. **Broadcast** — Signed transaction submission to base node via HTTP RPC

For batch sends (`run-pool` mode), the new wallet supports 1-to-many transactions where a single tx pays multiple recipients, exercising the `send_batch_transaction` API.

## Architecture

```
src/
├── main.rs              # CLI entry point, orchestration, scan measurement, system monitors
├── config.rs            # CLI args (Cli, Command, WalletConfig, SetupConfig, SetupState)
├── address_pool.rs      # Generate/load/persist 1000+ Tari addresses for round-robin sends
├── driver/
│   ├── mod.rs           # WalletDriver trait (send, batch send, balance, split, utxos, sync)
│   ├── old_wallet_driver.rs  # gRPC driver using Transfer RPC (single call)
│   ├── old_wallet_process.rs # Spawns/manages minotari_console_wallet child process
│   └── new_wallet_driver.rs  # minotari-cli library driver (local DB + sign + broadcast)
├── scenarios/
│   ├── mod.rs           # Scenario trait (name, budget, run)
│   ├── pool_payout.rs   # Outbound flood: constant + burst (supports batch mode)
│   ├── inbound_flood.rs # Poisson sends + inbound detection monitoring
│   ├── bidirectional.rs # Ramp-up send + receive stress
│   ├── fragmentation.rs # UTXO cascade split + aggregation sends
│   └── lock_contention.rs # Rapid sequential sends measuring UTXO lock contention
├── load/
│   ├── mod.rs
│   └── patterns.rs      # LoadPattern: Constant, Ramp, Burst, Poisson
├── metrics/
│   ├── recorder.rs      # TransactionRecord, ScanRecord, SystemSnapshot, MetricsCollector
│   ├── system_monitor.rs # Background CPU/memory/balance sampling
│   └── reporter.rs      # CSV output + console comparison summary
└── fund_management/
    ├── splitter.rs       # Cascading UTXO splits with metric recording
    ├── consolidator.rs   # Sweep to single UTXO with tx status polling
    ├── drainer.rs        # Drain address pool sink wallets back to main wallet
    └── reconciler.rs     # Inter-scenario balance verification
```