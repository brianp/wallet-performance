use std::time::Instant;

use chrono::Utc;
use log::{debug, info, warn};
use rand::{rngs::StdRng, Rng, SeedableRng};
use tokio::time::{sleep, Duration};

use crate::address_pool::AddressPool;
use crate::driver::{SendResult, WalletDriver};
use crate::metrics::recorder::{MetricsCollector, TransactionRecord};

/// Defines a load pattern for generating transactions.
#[derive(Debug, Clone)]
pub enum LoadPattern {
    /// Constant rate: `tps` transactions per second for `duration_secs`.
    Constant { tps: f64, duration_secs: u64 },
    /// Ramp: start at `start_tps`, increase by `step_tps` every `step_interval_secs`.
    Ramp {
        start_tps: f64,
        max_tps: f64,
        step_tps: f64,
        step_interval_secs: u64,
        duration_secs: u64,
    },
    /// Burst: send `count` transactions as fast as possible.
    Burst { count: u64 },
    /// Poisson: random intervals averaging `avg_tps` for `duration_secs`.
    Poisson { avg_tps: f64, duration_secs: u64 },
}

/// Generates load according to a pattern, sending transactions and recording metrics.
pub struct LoadGenerator;

impl LoadGenerator {
    /// Execute a load pattern against a wallet driver.
    pub async fn execute(
        pattern: &LoadPattern,
        wallet: &dyn WalletDriver,
        address_pool: &AddressPool,
        amount: u64,
        scenario_name: &str,
        collector: &MetricsCollector,
    ) -> anyhow::Result<Vec<SendResult>> {
        match pattern {
            LoadPattern::Constant { tps, duration_secs } => {
                let interval = Duration::from_secs_f64(1.0 / tps);
                let deadline = Instant::now() + Duration::from_secs(*duration_secs);
                let mut results = Vec::new();

                info!(
                    "[{}] Constant load: {:.1} TPS for {}s",
                    wallet.name(),
                    tps,
                    duration_secs
                );

                while Instant::now() < deadline {
                    let result = Self::send_and_record(
                        wallet,
                        address_pool.next_address(),
                        amount,
                        scenario_name,
                        collector,
                    )
                    .await;
                    results.push(result);
                    sleep(interval).await;
                }

                Ok(results)
            }
            LoadPattern::Ramp {
                start_tps,
                max_tps,
                step_tps,
                step_interval_secs,
                duration_secs,
            } => {
                let deadline = Instant::now() + Duration::from_secs(*duration_secs);
                let mut results = Vec::new();
                let mut current_tps = *start_tps;
                let mut last_step = Instant::now();

                info!(
                    "[{}] Ramp load: {:.1} -> {:.1} TPS over {}s",
                    wallet.name(),
                    start_tps,
                    max_tps,
                    duration_secs
                );

                while Instant::now() < deadline {
                    if last_step.elapsed() > Duration::from_secs(*step_interval_secs)
                        && current_tps < *max_tps
                    {
                        current_tps = (current_tps + step_tps).min(*max_tps);
                        last_step = Instant::now();
                        debug!("Ramp: TPS increased to {:.1}", current_tps);
                    }

                    let interval = Duration::from_secs_f64(1.0 / current_tps);
                    let result = Self::send_and_record(
                        wallet,
                        address_pool.next_address(),
                        amount,
                        scenario_name,
                        collector,
                    )
                    .await;
                    results.push(result);
                    sleep(interval).await;
                }

                Ok(results)
            }
            LoadPattern::Burst { count } => {
                let mut results = Vec::new();

                info!("[{}] Burst load: {} transactions", wallet.name(), count);

                for i in 0..*count {
                    let result = Self::send_and_record(
                        wallet,
                        address_pool.next_address(),
                        amount,
                        scenario_name,
                        collector,
                    )
                    .await;
                    if !result.accepted {
                        warn!("Burst tx {} failed: {:?}", i, result.error);
                    }
                    results.push(result);
                }

                Ok(results)
            }
            LoadPattern::Poisson {
                avg_tps,
                duration_secs,
            } => {
                let deadline = Instant::now() + Duration::from_secs(*duration_secs);
                let mut results = Vec::new();
                let mut rng = StdRng::from_entropy();

                info!(
                    "[{}] Poisson load: avg {:.1} TPS for {}s",
                    wallet.name(),
                    avg_tps,
                    duration_secs
                );

                while Instant::now() < deadline {
                    // Exponential distribution for inter-arrival times
                    let u: f64 = rng.gen_range(0.001..1.0);
                    let wait_secs = -u.ln() / avg_tps;
                    sleep(Duration::from_secs_f64(wait_secs)).await;

                    if Instant::now() >= deadline {
                        break;
                    }

                    let result = Self::send_and_record(
                        wallet,
                        address_pool.next_address(),
                        amount,
                        scenario_name,
                        collector,
                    )
                    .await;
                    results.push(result);
                }

                Ok(results)
            }
        }
    }

    async fn send_and_record(
        wallet: &dyn WalletDriver,
        recipient: &str,
        amount: u64,
        scenario_name: &str,
        collector: &MetricsCollector,
    ) -> SendResult {
        let start = Utc::now();
        let timer = Instant::now();

        let result = wallet.send_transaction(recipient, amount).await;
        let duration = timer.elapsed();
        let end = Utc::now();

        let (send_result, error) = match result {
            Ok(r) => (r, None),
            Err(e) => (
                SendResult {
                    tx_id: String::new(),
                    accepted: false,
                    error: Some(e.to_string()),
                    fee: None,
                },
                Some(e.to_string()),
            ),
        };

        collector.record_transaction(TransactionRecord {
            wallet: wallet.name().to_string(),
            scenario: scenario_name.to_string(),
            tx_id: send_result.tx_id.clone(),
            start_time: start,
            end_time: end,
            duration_ms: duration.as_millis() as u64,
            accepted: send_result.accepted,
            error: send_result.error.clone().or(error),
            amount,
            fee: send_result.fee,
            tx_type: "send".to_string(),
        });

        send_result
    }
}
