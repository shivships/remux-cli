use std::path::{Path, PathBuf};
use std::process::Stdio;

use regex::Regex;
use reqwest::Client;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

const TUNNEL_TIMEOUT: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const KEEPALIVE_FAIL_THRESHOLD: u32 = 3;

const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub enum TunnelState {
    Connected {
        full_url: String,
        display_url: String,
    },
    Reconnecting,
}

pub struct TunnelManager {
    state_tx: watch::Sender<TunnelState>,
    cloudflared_bin: PathBuf,
    port: u16,
    base_url: String,
    display_base_url: String,
}

impl TunnelManager {
    /// Create and start the tunnel manager. Returns a watch receiver for UI updates.
    pub fn start(
        child: Child,
        tunnel_url: String,
        stderr_handle: JoinHandle<()>,
        cloudflared_bin: PathBuf,
        port: u16,
        base_url: String,
        display_base_url: String,
    ) -> watch::Receiver<TunnelState> {
        let slug = extract_slug(&tunnel_url);
        let initial_state = TunnelState::Connected {
            full_url: format!("{}/{}", base_url, slug),
            display_url: format!("{}/{}", display_base_url, slug),
        };

        let (state_tx, state_rx) = watch::channel(initial_state);

        let manager = Self {
            state_tx,
            cloudflared_bin,
            port,
            base_url,
            display_base_url,
        };

        tokio::spawn(manager.run(child, tunnel_url, stderr_handle));

        state_rx
    }

    async fn run(
        self,
        mut child: Child,
        mut tunnel_url: String,
        mut stderr_handle: JoinHandle<()>,
    ) {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();

        loop {
            // === MONITOR PHASE ===
            let mut consecutive_failures: u32 = 0;
            let mut keepalive_interval = tokio::time::interval(KEEPALIVE_INTERVAL);
            // Skip the immediate first tick
            keepalive_interval.tick().await;

            loop {
                tokio::select! {
                    _ = &mut stderr_handle => {
                        warn!("cloudflared process exited");
                        break;
                    }
                    _ = keepalive_interval.tick() => {
                        match client.get(&tunnel_url).send().await {
                            Ok(resp) if resp.status().is_success() => {
                                if consecutive_failures > 0 {
                                    info!("Tunnel keepalive recovered after {} failures", consecutive_failures);
                                }
                                consecutive_failures = 0;
                            }
                            _ => {
                                consecutive_failures += 1;
                                warn!(failures = consecutive_failures, "Tunnel keepalive failed");
                                if consecutive_failures >= KEEPALIVE_FAIL_THRESHOLD {
                                    warn!("Tunnel appears dead, killing cloudflared");
                                    child.kill().await.ok();
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // === RECONNECT PHASE ===
            child.kill().await.ok();
            stderr_handle.abort();
            let _ = self.state_tx.send(TunnelState::Reconnecting);

            let mut backoff = BACKOFF_INITIAL;
            loop {
                info!(backoff_secs = backoff.as_secs(), "Attempting tunnel reconnect");

                match spawn_tunnel(&self.cloudflared_bin, self.port).await {
                    Ok((new_child, new_tunnel_url, new_stderr_handle)) => {
                        let slug = extract_slug(&new_tunnel_url);
                        let _ = self.state_tx.send(TunnelState::Connected {
                            full_url: format!("{}/{}", self.base_url, slug),
                            display_url: format!("{}/{}", self.display_base_url, slug),
                        });
                        info!("Tunnel reconnected");

                        child = new_child;
                        tunnel_url = new_tunnel_url;
                        stderr_handle = new_stderr_handle;
                        break;
                    }
                    Err(e) => {
                        warn!("Reconnect failed: {}", e);
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(BACKOFF_CAP);
                    }
                }
            }
        }
    }
}

fn extract_slug(tunnel_url: &str) -> String {
    tunnel_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('.')
        .next()
        .unwrap_or("")
        .to_string()
}

pub async fn spawn_tunnel(
    cloudflared_bin: &Path,
    port: u16,
) -> anyhow::Result<(Child, String, JoinHandle<()>)> {
    info!(port, path = %cloudflared_bin.display(), "Starting cloudflared tunnel");

    let mut child = Command::new(cloudflared_bin)
        .args(["tunnel", "--url", &format!("http://127.0.0.1:{}", port)])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to run cloudflared: {}", e))?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to capture cloudflared stderr"))?;

    let result = timeout(TUNNEL_TIMEOUT, parse_tunnel_url(stderr)).await;

    let (url, reader) = match result {
        Ok(inner) => inner?,
        Err(_) => {
            child.kill().await.ok();
            anyhow::bail!(
                "Tunnel creation timed out — cloudflared did not return a URL within {}s",
                TUNNEL_TIMEOUT.as_secs()
            );
        }
    };

    let stderr_handle = tokio::spawn(async move {
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            let lower = trimmed.to_ascii_lowercase();
            if lower.contains("err")
                || lower.contains("failed")
                || lower.contains("unavailable")
                || lower.contains("refused")
            {
                warn!("cloudflared: {}", trimmed);
            } else {
                debug!(line = %trimmed, "cloudflared");
            }
        }
    });

    info!(%url, "Tunnel ready");
    Ok((child, url, stderr_handle))
}

async fn parse_tunnel_url(
    stderr: tokio::process::ChildStderr,
) -> anyhow::Result<(String, BufReader<tokio::process::ChildStderr>)> {
    let mut reader = BufReader::new(stderr);
    // Capture tunnel URLs but not the API endpoint (api.trycloudflare.com)
    // which appears in error messages. The URL must not be followed by a path.
    let re = Regex::new(r"(https://[a-zA-Z0-9\-]+\.trycloudflare\.com)(?:[^/a-zA-Z0-9]|$)")?;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("cloudflared exited without providing a tunnel URL");
        }
        let trimmed = line.trim();
        debug!(line = %trimmed, "cloudflared");
        if let Some(caps) = re.captures(trimmed) {
            let url = caps[1].to_string();
            if !url.starts_with("https://api.") {
                return Ok((url, reader));
            }
        }
    }
}
