# Wallet Performance Comparison Test

Performance test harness comparing two transaction flows:

- **Old wallet** — `minotari_console_wallet` via gRPC `Transfer` RPC (wallet handles signing internally)
- **New wallet** — `minotari-cli` library: local UTXO selection → `sign_locked_transaction` → broadcast via HTTP RPC

Both exercise fundamentally different code paths. The new flow is the offline signing model used by Tari Universe / minotari-cli.

## Overview

This tool hammers both transaction paths with identical workloads and compares:

- **Throughput** — Transactions per second (TPS) with constant, ramp, burst, and Poisson load patterns
- **Latency** — p50, p95, p99 response times per scenario
- **Error rates** — Percentage of rejected/failed transactions, UTXO lock contention
- **Fee efficiency** — Cumulative fee tracking per wallet, per scenario
- **UTXO handling** — Splitting, fragmentation, input aggregation performance
- **Concurrency** — Lock contention under rapid sequential sends (10/25/50 batches)
- **System resources** — CPU and memory monitoring throughout test runs
- **Balance reconciliation** — Automated inter-scenario and final balance verification

## Prerequisites

1. **`minotari_console_wallet`** running with gRPC enabled (old wallet)
2. Both wallets funded with testnet tXTM (minimum 100,000 tXTM each recommended)
3. A **base node HTTP RPC** endpoint for broadcasting new wallet transactions

### Old wallet setup

```bash
minotari_console_wallet --network esmeralda --grpc-enabled --grpc-address 127.0.0.1:18143
```

## Building

```bash
cd wallet-performance
cargo build --release
```

## Usage

### 1. Setup (automated)

The `setup` command creates the new wallet, displays both wallet addresses, and waits for funding:

```bash
cargo run -- setup \
  --old-wallet-grpc http://127.0.0.1:18143 \
  --network esmeralda
```

This will:
1. Create a `data/` directory with a new minotari-cli wallet (auto-generated seed words)
2. Connect to the old wallet via gRPC to get its address
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
1. Load or generate the address pool (1000 unique Tari addresses by default)
2. Run baseline measurements and record initial balances
3. Start background system monitors (CPU/memory sampling every 30s)
4. Run **all scenarios on the old wallet first**, then **all scenarios on the new wallet** (sequential, no cross-contamination)
5. Run inter-scenario balance reconciliation checkpoints between each
6. Stop system monitors
7. Verify final balances against initial - cumulative fees
8. Write CSV reports and print comparison summary

### 3. Run a single scenario

```bash
cargo run -- run pool-payout
```

Available scenarios:

| Scenario | Description | Budget |
|----------|-------------|--------|
| `pool-payout` | Split UTXOs → constant 5 TPS for 2 min → burst 50 txs | 40,000 tXTM |
| `inbound-flood` | Poisson-rate sends + monitor inbound detection for 10 min | 20,000 tXTM |
| `bidirectional` | Ramp send rate from 1/min to 5/min over 30 min | 20,000 tXTM |
| `fragmentation` | Cascade split into 500 UTXOs → test aggregation sends | 10,000 tXTM |
| `lock-contention` | 10/25/50 rapid sequential sends measuring contention | 10,000 tXTM |

### 4. Consolidate UTXOs after testing

```bash
cargo run -- consolidate
```

Sweeps all UTXOs to a single output per wallet, polls for mining confirmation, and reports final balances.

## Configuration Options

| Flag | Default | Description |
|------|---------|-------------|
| `--old-wallet-grpc` | `http://127.0.0.1:18143` | Old wallet gRPC endpoint |
| `--fee-per-gram` | `5` | Fee per gram in MicroMinotari (old wallet) |
| `--new-wallet-db` | `data/new_wallet.sqlite` | Path to minotari-cli SQLite database |
| `--new-wallet-account` | `default` | Account name in minotari-cli wallet |
| `--new-wallet-password` | `""` (env: `TARI_WALLET_PASSWORD`) | Password for minotari-cli account |
| `--new-wallet-seed-words` | — (env: `TARI_SEED_WORDS`) | Space-separated 24-word mnemonic |
| `--base-node-url` | `https://rpc.tari.com` | Base node HTTP RPC for broadcasting (new wallet) |
| `--network` | `esmeralda` | Network (mainnet, nextnet, esmeralda, localnet) |
| `--block-time-secs` | `120` | Block time for confirmation calculations |
| `--confirmations` | `3` | Confirmations to wait between phases |
| `--address-pool-size` | `1000` | Number of receive-only sink addresses to generate |
| `--new-wallet-seed-words` | auto from `data/setup.json` | Seed words (auto-loaded after `setup`) |
| `--address-pool-file` | `data/address_pool.json` | Path to address pool JSON (loaded if exists, generated otherwise) |
| `--output-dir` | `results` | Directory for CSV output files |
| `--log-level` | `info` | Log verbosity (error/warn/info/debug/trace) |

## Output

Results are written to the output directory as CSV files:

- **`transactions.csv`** — Per-transaction timing data: wallet, scenario, duration_ms, accepted, error, amount, fee, tx_type
- **`system_snapshots.csv`** — Periodic CPU/memory/balance samples per wallet
- **`summary.csv`** — Aggregated stats per scenario per wallet: TPS, p50/p95/p99, error rate, total fees

A comparison summary is printed to the console after each run.

## Transaction Flow Comparison

### Old Wallet (gRPC Transfer)

```
Client → Transfer gRPC → wallet signs internally → broadcast → done
```

One gRPC call handles everything: UTXO selection, signing, and broadcast.

### New Wallet (minotari-cli library)

```
Client → TransactionSender.start_new_transaction() → unsigned tx (UTXO selection + locking)
         ↓
Client → sign_locked_transaction() → signed tx (local key derivation from seed words)
         ↓
Client → TransactionSender.finalize_transaction_and_broadcast() → HTTP RPC to base node → done
```

Three distinct phases using the `minotari` crate directly:
1. **Prepare** — UTXO selection from SQLite, fee estimation, transaction construction, UTXO locking
2. **Sign** — Key derivation from seed words, transaction signing (all in-process, no external binary)
3. **Broadcast** — Signed transaction submission to base node via HTTP RPC

## Architecture

```
src/
├── main.rs              # CLI entry point, orchestration, system monitors, reconciliation
├── config.rs            # CLI args and wallet configuration
├── address_pool.rs      # Generate/load/persist 1000+ Tari addresses for round-robin sends
├── driver/
│   ├── mod.rs           # WalletDriver trait (send, balance, split, utxos, status)
│   ├── old_wallet_driver.rs  # gRPC driver using Transfer RPC (single call)
│   └── new_wallet_driver.rs  # minotari-cli library driver (local DB + sign + broadcast)
├── scenarios/
│   ├── mod.rs           # Scenario trait (name, budget, run)
│   ├── pool_payout.rs   # Outbound flood: constant + burst
│   ├── inbound_flood.rs # Poisson sends + inbound detection monitoring
│   ├── bidirectional.rs # Ramp-up send + receive stress
│   ├── fragmentation.rs # UTXO cascade split + aggregation sends
│   └── lock_contention.rs # Rapid sequential sends measuring UTXO lock contention
├── load/
│   └── patterns.rs      # LoadPattern: Constant, Ramp, Burst, Poisson
├── metrics/
│   ├── recorder.rs      # TransactionRecord, SystemSnapshot, MetricsCollector, ScenarioStats
│   ├── system_monitor.rs # Background CPU/memory/balance sampling
│   └── reporter.rs      # CSV output + console comparison summary
└── fund_management/
    ├── splitter.rs       # Cascading UTXO splits with metric recording
    ├── consolidator.rs   # Sweep to single UTXO with tx status polling
    └── reconciler.rs     # Inter-scenario balance verification
```
