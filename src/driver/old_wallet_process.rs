use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use log::{info, warn};
use minotari_wallet_grpc_client::{grpc, WalletGrpcClient};
use tokio::time::sleep;

/// Configuration for spawning a minotari_console_wallet process.
pub struct OldWalletProcessConfig {
    pub binary_path: PathBuf,
    pub base_dir: PathBuf,
    pub grpc_port: u16,
    pub network: String,
    pub seed_words: String,
    pub password: String,
}

/// Manages the lifecycle of a minotari_console_wallet child process.
pub struct OldWalletProcess {
    child: Option<Child>,
    pub grpc_addr: String,
    spawn_time: Instant,
}

impl OldWalletProcess {
    /// Spawn a new minotari_console_wallet process and wait for gRPC to become ready.
    pub async fn spawn(config: &OldWalletProcessConfig) -> anyhow::Result<Self> {
        // Ensure base dir exists
        std::fs::create_dir_all(&config.base_dir).with_context(|| {
            format!(
                "Creating old wallet base dir: {}",
                config.base_dir.display()
            )
        })?;

        let grpc_multiaddr = format!("/ip4/127.0.0.1/tcp/{}", config.grpc_port);
        let grpc_url = format!("http://127.0.0.1:{}", config.grpc_port);

        info!(
            "[old_wallet_process] Spawning {} with base_dir={}, grpc={}",
            config.binary_path.display(),
            config.base_dir.display(),
            grpc_multiaddr,
        );

        // The old wallet uses short network names (e.g. "esme" not "esmeralda")
        let network_lower = config.network.to_lowercase();
        let network_arg = match network_lower.as_str() {
            "esmeralda" => "esme",
            other => other,
        };

        let mut cmd = Command::new(&config.binary_path);
        cmd.arg("-b")
            .arg(&config.base_dir)
            .arg("--network")
            .arg(network_arg)
            .arg("--grpc-enabled")
            .arg("--grpc-address")
            .arg(&grpc_multiaddr)
            .arg("--password")
            .arg(&config.password)
            .arg("--seed-words")
            .arg(&config.seed_words)
            .arg("--non-interactive-mode")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let spawn_time = Instant::now();
        let child = cmd.spawn().with_context(|| {
            format!(
                "Failed to spawn minotari_console_wallet at '{}'",
                config.binary_path.display()
            )
        })?;

        let pid = child.id();
        info!("[old_wallet_process] Spawned with PID {}", pid);

        let mut process = Self {
            child: Some(child),
            grpc_addr: grpc_url,
            spawn_time,
        };

        // Wait for gRPC to become ready
        process.wait_for_grpc_ready().await?;

        Ok(process)
    }

    /// Delete the old wallet data directory for a fresh start.
    pub fn clean_data_dir(base_dir: &std::path::Path) -> anyhow::Result<()> {
        if base_dir.exists() {
            info!(
                "[old_wallet_process] Deleting old wallet data dir: {}",
                base_dir.display()
            );
            std::fs::remove_dir_all(base_dir).with_context(|| {
                format!(
                    "Failed to delete old wallet data dir: {}",
                    base_dir.display()
                )
            })?;
        }
        Ok(())
    }

    /// Poll until gRPC connects successfully.
    async fn wait_for_grpc_ready(&mut self) -> anyhow::Result<()> {
        let max_wait = Duration::from_secs(3600); // 1 hour — recovery scan can take a while
        let mut backoff = Duration::from_millis(100);
        let max_backoff = Duration::from_secs(5);
        let start = Instant::now();

        info!(
            "[old_wallet_process] Waiting for gRPC at {} (max {}s)...",
            self.grpc_addr,
            max_wait.as_secs()
        );

        loop {
            // Check child is still alive
            if let Some(ref mut child) = self.child {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        self.child = None;
                        bail!(
                            "minotari_console_wallet exited prematurely with status: {}",
                            status
                        );
                    }
                    Ok(None) => {} // still running
                    Err(e) => warn!("[old_wallet_process] Error checking child status: {}", e),
                }
            }

            if let Ok(mut client) = WalletGrpcClient::connect(&self.grpc_addr).await {
                if client
                    .get_balance(grpc::GetBalanceRequest { payment_id: None })
                    .await
                    .is_ok()
                {
                    info!(
                        "[old_wallet_process] gRPC ready after {:.1}s",
                        start.elapsed().as_secs_f64()
                    );
                    return Ok(());
                }
            }

            if start.elapsed() > max_wait {
                bail!(
                    "Timed out after {}s waiting for old wallet gRPC at {}",
                    max_wait.as_secs(),
                    self.grpc_addr,
                );
            }

            sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    }

    /// Wait for the old wallet to complete its blockchain scan.
    /// Returns the total duration from process spawn to scan complete.
    pub async fn wait_for_scan_complete(&self) -> anyhow::Result<Duration> {
        let max_wait = Duration::from_secs(3600); // 1 hour max
        let poll_interval = Duration::from_secs(10);
        let stable_threshold = Duration::from_secs(30);
        let start = Instant::now();

        info!("[old_wallet_process] Waiting for blockchain scan to complete...");

        let mut last_balance: Option<u64> = None;
        let mut last_change = Instant::now();
        let mut seen_positive_balance = false;

        loop {
            if start.elapsed() > max_wait {
                warn!(
                    "[old_wallet_process] Scan timed out after {}s",
                    max_wait.as_secs()
                );
                break;
            }

            match WalletGrpcClient::connect(&self.grpc_addr).await {
                Ok(mut client) => {
                    match client
                        .get_balance(grpc::GetBalanceRequest { payment_id: None })
                        .await
                    {
                        Ok(response) => {
                            let balance = response.into_inner();
                            let available = balance.available_balance;
                            let pending = balance.pending_incoming_balance;
                            let total = available + pending;

                            println!(
                                "  [old_wallet scan] available={} tXTM, pending={} tXTM ({}s elapsed)",
                                available / 1_000_000,
                                pending / 1_000_000,
                                self.spawn_time.elapsed().as_secs(),
                            );

                            if total > 0 {
                                seen_positive_balance = true;
                            }

                            if let Some(prev) = last_balance {
                                if prev != available {
                                    last_change = Instant::now();
                                } else if seen_positive_balance
                                    && last_change.elapsed() > stable_threshold
                                {
                                    info!(
                                        "[old_wallet_process] Balance stable for {}s, scan complete",
                                        stable_threshold.as_secs()
                                    );
                                    break;
                                }
                            }
                            last_balance = Some(available);
                        }
                        Err(e) => {
                            warn!("[old_wallet_process] Balance query failed: {}", e);
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "[old_wallet_process] gRPC connect failed during scan wait: {}",
                        e
                    );
                }
            }

            sleep(poll_interval).await;
        }

        let total_duration = self.spawn_time.elapsed();
        info!(
            "[old_wallet_process] Scan measurement: {:.1}s from spawn",
            total_duration.as_secs_f64()
        );
        Ok(total_duration)
    }

    /// Kill the child process.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            info!("[old_wallet_process] Killing process PID {}", pid);

            // Try graceful SIGTERM first (on Unix)
            #[cfg(unix)]
            {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
                // Give it a few seconds to shut down gracefully
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => return,
                        Ok(None) => {
                            if Instant::now() > deadline {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Err(_) => break,
                    }
                }
            }

            // Force kill
            if let Err(e) = child.kill() {
                warn!("[old_wallet_process] Failed to kill process: {}", e);
            }
            let _ = child.wait();
        }
    }
}

impl Drop for OldWalletProcess {
    fn drop(&mut self) {
        self.kill();
    }
}
