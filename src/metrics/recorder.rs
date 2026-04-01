use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// A single transaction timing record.
#[derive(Debug, Clone, Serialize)]
pub struct TransactionRecord {
    pub wallet: String,
    pub scenario: String,
    pub tx_id: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub duration_ms: u64,
    pub accepted: bool,
    pub error: Option<String>,
    pub amount: u64,
    pub fee: Option<u64>,
    pub tx_type: String,
}

/// A blockchain scanning measurement record.
#[derive(Debug, Clone, Serialize)]
pub struct ScanRecord {
    pub wallet: String,
    pub scan_type: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub duration_ms: u64,
    pub start_height: Option<u64>,
    pub end_height: Option<u64>,
    pub blocks_scanned: Option<u64>,
}

/// A periodic system snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct SystemSnapshot {
    pub timestamp: DateTime<Utc>,
    pub wallet: String,
    pub memory_rss_bytes: u64,
    pub cpu_percent: f32,
    pub utxo_count: Option<u64>,
    pub balance_available: Option<u64>,
}

/// Collects all metrics across scenarios.
pub struct MetricsCollector {
    transactions: Mutex<Vec<TransactionRecord>>,
    snapshots: Mutex<Vec<SystemSnapshot>>,
    scan_records: Mutex<Vec<ScanRecord>>,
    cumulative_fees: Mutex<u64>,
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self {
            transactions: Mutex::new(Vec::new()),
            snapshots: Mutex::new(Vec::new()),
            scan_records: Mutex::new(Vec::new()),
            cumulative_fees: Mutex::new(0),
        }
    }

    /// Record a transaction timing.
    pub fn record_transaction(&self, record: TransactionRecord) {
        if let Some(fee) = record.fee {
            *self.cumulative_fees.lock().unwrap() += fee;
        }
        self.transactions.lock().unwrap().push(record);
    }

    /// Record a system snapshot.
    pub fn record_snapshot(&self, snapshot: SystemSnapshot) {
        self.snapshots.lock().unwrap().push(snapshot);
    }

    /// Record a blockchain scan measurement.
    pub fn record_scan(&self, record: ScanRecord) {
        self.scan_records.lock().unwrap().push(record);
    }

    /// Get all scan records.
    pub fn get_scan_records(&self) -> Vec<ScanRecord> {
        self.scan_records.lock().unwrap().clone()
    }

    /// Get total fees spent so far.
    pub fn total_fees(&self) -> u64 {
        *self.cumulative_fees.lock().unwrap()
    }

    /// Get all transaction records.
    pub fn get_transactions(&self) -> Vec<TransactionRecord> {
        self.transactions.lock().unwrap().clone()
    }

    /// Get all system snapshots.
    pub fn get_snapshots(&self) -> Vec<SystemSnapshot> {
        self.snapshots.lock().unwrap().clone()
    }

    /// Get transaction records filtered by scenario.
    pub fn get_transactions_for_scenario(&self, scenario: &str) -> Vec<TransactionRecord> {
        self.transactions
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.scenario == scenario)
            .cloned()
            .collect()
    }

    /// Calculate summary stats for a set of transaction records.
    pub fn calculate_stats(records: &[TransactionRecord]) -> ScenarioStats {
        if records.is_empty() {
            return ScenarioStats::default();
        }

        let accepted: Vec<&TransactionRecord> = records.iter().filter(|r| r.accepted).collect();
        let failed: Vec<&TransactionRecord> = records.iter().filter(|r| !r.accepted).collect();

        let mut durations: Vec<u64> = accepted.iter().map(|r| r.duration_ms).collect();
        durations.sort();

        let total_count = records.len();
        let accepted_count = accepted.len();
        let error_count = failed.len();

        let (p50, p95, p99) = if !durations.is_empty() {
            let p50_idx = durations.len() * 50 / 100;
            let p95_idx = (durations.len() * 95 / 100).min(durations.len() - 1);
            let p99_idx = (durations.len() * 99 / 100).min(durations.len() - 1);
            (durations[p50_idx], durations[p95_idx], durations[p99_idx])
        } else {
            (0, 0, 0)
        };

        let total_duration_ms: u64 = accepted.iter().map(|r| r.duration_ms).sum();
        let first_start = records.iter().map(|r| r.start_time).min();
        let last_end = records.iter().map(|r| r.end_time).max();
        let wall_clock_secs = match (first_start, last_end) {
            (Some(s), Some(e)) => (e - s).num_seconds().max(1) as f64,
            _ => 1.0,
        };
        let tps = accepted_count as f64 / wall_clock_secs;

        let total_fees: u64 = accepted.iter().filter_map(|r| r.fee).sum();

        ScenarioStats {
            total_count,
            accepted_count,
            error_count,
            error_rate: error_count as f64 / total_count as f64,
            tps,
            latency_p50_ms: p50,
            latency_p95_ms: p95,
            latency_p99_ms: p99,
            avg_latency_ms: if accepted_count > 0 {
                total_duration_ms / accepted_count as u64
            } else {
                0
            },
            total_fees,
        }
    }
}

/// Summary statistics for a scenario run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScenarioStats {
    pub total_count: usize,
    pub accepted_count: usize,
    pub error_count: usize,
    pub error_rate: f64,
    pub tps: f64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub latency_p99_ms: u64,
    pub avg_latency_ms: u64,
    pub total_fees: u64,
}
