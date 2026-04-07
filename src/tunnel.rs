use std::process::Stdio;

use regex::Regex;
use reqwest::Client;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

const TUNNEL_TIMEOUT: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const KEEPALIVE_FAIL_THRESHOLD: u32 = 3;

pub async fn spawn_tunnel(port: u16) -> anyhow::Result<(Child, String)> {
    let which = Command::new("which")
        .arg("cloudflared")
        .output()
        .await;

    if which.is_err() || !which.unwrap().status.success() {
        anyhow::bail!(
            "cloudflared not found on PATH. Install it: https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/"
        );
    }

    info!(port, "Starting cloudflared tunnel");

    let mut child = Command::new("cloudflared")
        .args(["tunnel", "--url", &format!("http://127.0.0.1:{}", port)])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to capture cloudflared stderr"))?;

    let (url, reader) = timeout(TUNNEL_TIMEOUT, parse_tunnel_url(stderr))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Timed out waiting for cloudflared tunnel URL after {}s",
                TUNNEL_TIMEOUT.as_secs()
            )
        })??;

    tokio::spawn(async move {
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
        warn!("cloudflared process exited — tunnel is no longer available");
    });

    info!(%url, "Tunnel ready");
    Ok((child, url))
}

pub fn spawn_keepalive(url: String) {
    tokio::spawn(async move {
        let client = match Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to create keepalive HTTP client: {}", e);
                return;
            }
        };

        let mut consecutive_failures: u32 = 0;

        loop {
            tokio::time::sleep(KEEPALIVE_INTERVAL).await;

            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if consecutive_failures > 0 {
                        info!(
                            "Tunnel keepalive recovered after {} failures",
                            consecutive_failures
                        );
                    }
                    consecutive_failures = 0;
                }
                Ok(resp) => {
                    consecutive_failures += 1;
                    warn!(
                        status = %resp.status(),
                        failures = consecutive_failures,
                        "Tunnel keepalive got unexpected status"
                    );
                }
                Err(e) => {
                    consecutive_failures += 1;
                    warn!(failures = consecutive_failures, "Tunnel keepalive failed: {}", e);
                }
            }

            if consecutive_failures >= KEEPALIVE_FAIL_THRESHOLD {
                warn!(
                    "Tunnel appears to be down ({} consecutive failures)",
                    consecutive_failures
                );
            }
        }
    });
}

async fn parse_tunnel_url(
    stderr: tokio::process::ChildStderr,
) -> anyhow::Result<(String, BufReader<tokio::process::ChildStderr>)> {
    let mut reader = BufReader::new(stderr);
    let re = Regex::new(r"https://[a-zA-Z0-9\-]+\.trycloudflare\.com")?;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("cloudflared exited without providing a tunnel URL");
        }
        let trimmed = line.trim();
        debug!(line = %trimmed, "cloudflared");
        if let Some(m) = re.find(trimmed) {
            return Ok((m.as_str().to_string(), reader));
        }
    }
}
