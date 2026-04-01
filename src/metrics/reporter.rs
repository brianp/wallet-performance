use std::fs;
use std::path::Path;

use anyhow::Context;
use log::info;

use super::recorder::{MetricsCollector, ScanRecord, SystemSnapshot, TransactionRecord};

/// Generates CSV reports from collected metrics.
pub struct Reporter;

impl Reporter {
    /// Write all collected data to CSV files in the output directory.
    pub fn write_reports(collector: &MetricsCollector, output_dir: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(output_dir)
            .with_context(|| format!("Creating output dir: {}", output_dir.display()))?;

        Self::write_transactions_csv(
            &collector.get_transactions(),
            &output_dir.join("transactions.csv"),
        )?;

        let snapshots = collector.get_snapshots();
        if !snapshots.is_empty() {
            Self::write_snapshots_csv(&snapshots, &output_dir.join("system_snapshots.csv"))?;
        }

        let scans = collector.get_scan_records();
        if !scans.is_empty() {
            Self::write_scans_csv(&scans, &output_dir.join("scans.csv"))?;
        }

        Self::write_summary(collector, &output_dir.join("summary.csv"))?;

        info!("Reports written to {}", output_dir.display());
        Ok(())
    }

    fn write_transactions_csv(records: &[TransactionRecord], path: &Path) -> anyhow::Result<()> {
        let mut writer =
            csv::Writer::from_path(path).with_context(|| format!("Creating {}", path.display()))?;

        for record in records {
            writer.serialize(record)?;
        }
        writer.flush()?;
        info!(
            "Wrote {} transaction records to {}",
            records.len(),
            path.display()
        );
        Ok(())
    }

    fn write_scans_csv(records: &[ScanRecord], path: &Path) -> anyhow::Result<()> {
        let mut writer =
            csv::Writer::from_path(path).with_context(|| format!("Creating {}", path.display()))?;

        for record in records {
            writer.serialize(record)?;
        }
        writer.flush()?;
        info!("Wrote {} scan records to {}", records.len(), path.display());
        Ok(())
    }

    fn write_snapshots_csv(snapshots: &[SystemSnapshot], path: &Path) -> anyhow::Result<()> {
        let mut writer =
            csv::Writer::from_path(path).with_context(|| format!("Creating {}", path.display()))?;

        for snapshot in snapshots {
            writer.serialize(snapshot)?;
        }
        writer.flush()?;
        info!("Wrote {} snapshots to {}", snapshots.len(), path.display());
        Ok(())
    }

    fn write_summary(collector: &MetricsCollector, path: &Path) -> anyhow::Result<()> {
        let all_txs = collector.get_transactions();
        let scenarios: Vec<String> = {
            let mut s: Vec<String> = all_txs.iter().map(|t| t.scenario.clone()).collect();
            s.sort();
            s.dedup();
            s
        };

        let mut writer =
            csv::Writer::from_path(path).with_context(|| format!("Creating {}", path.display()))?;

        writer.write_record([
            "scenario",
            "wallet",
            "total_txs",
            "accepted",
            "errors",
            "error_rate",
            "tps",
            "p50_ms",
            "p95_ms",
            "p99_ms",
            "avg_ms",
            "total_fees",
        ])?;

        for scenario in &scenarios {
            let scenario_txs = collector.get_transactions_for_scenario(scenario);

            for wallet_name in &["old_wallet", "new_wallet"] {
                let wallet_txs: Vec<TransactionRecord> = scenario_txs
                    .iter()
                    .filter(|t| t.wallet == *wallet_name)
                    .cloned()
                    .collect();

                if wallet_txs.is_empty() {
                    continue;
                }

                let stats = MetricsCollector::calculate_stats(&wallet_txs);

                writer.write_record([
                    scenario.as_str(),
                    wallet_name,
                    &stats.total_count.to_string(),
                    &stats.accepted_count.to_string(),
                    &stats.error_count.to_string(),
                    &format!("{:.4}", stats.error_rate),
                    &format!("{:.2}", stats.tps),
                    &stats.latency_p50_ms.to_string(),
                    &stats.latency_p95_ms.to_string(),
                    &stats.latency_p99_ms.to_string(),
                    &stats.avg_latency_ms.to_string(),
                    &stats.total_fees.to_string(),
                ])?;
            }
        }

        writer.flush()?;

        info!(
            "Total cumulative fees: {} MicroMinotari",
            collector.total_fees()
        );

        Ok(())
    }

    /// Print a console summary comparing both wallets side-by-side.
    pub fn print_comparison(collector: &MetricsCollector) {
        let scan_records = collector.get_scan_records();
        if !scan_records.is_empty() {
            println!("\n{}", "=".repeat(90));
            println!("SCANNING PERFORMANCE");
            println!("{}", "=".repeat(90));
            for scan in &scan_records {
                let duration_secs = scan.duration_ms as f64 / 1000.0;
                println!(
                    "  {}: {:.1}s (type={}, blocks={:?})",
                    scan.wallet, duration_secs, scan.scan_type, scan.blocks_scanned,
                );
            }
            println!();
        }

        let all_txs = collector.get_transactions();
        let mut scenarios: Vec<String> = all_txs.iter().map(|t| t.scenario.clone()).collect();
        scenarios.sort();
        scenarios.dedup();

        println!("\n{}", "=".repeat(90));
        println!("PERFORMANCE COMPARISON SUMMARY");
        println!("{}\n", "=".repeat(90));

        for scenario in &scenarios {
            println!("--- {} ---", scenario);
            let scenario_txs = collector.get_transactions_for_scenario(scenario);

            for wallet_name in &["old_wallet", "new_wallet"] {
                let wallet_txs: Vec<TransactionRecord> = scenario_txs
                    .iter()
                    .filter(|t| t.wallet == *wallet_name)
                    .cloned()
                    .collect();

                if wallet_txs.is_empty() {
                    continue;
                }

                let stats = MetricsCollector::calculate_stats(&wallet_txs);
                println!(
                    "  {}: {} txs, TPS={:.2}, p50={}ms, p95={}ms, errors={:.1}%, fees={}",
                    wallet_name,
                    stats.total_count,
                    stats.tps,
                    stats.latency_p50_ms,
                    stats.latency_p95_ms,
                    stats.error_rate * 100.0,
                    stats.total_fees,
                );
            }
            println!();
        }

        println!(
            "Total cumulative fees: {} MicroMinotari",
            collector.total_fees()
        );
        println!("{}", "=".repeat(90));
    }
}
