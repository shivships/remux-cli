use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use tracing::{debug, info, warn};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB_RELEASES_URL: &str =
    "https://api.github.com/repos/shivships/remux-cli/releases/latest";
const CHECK_INTERVAL_SECS: u64 = 86400; // 24 hours

// --- Path helpers ---

fn bin_path() -> PathBuf {
    crate::config::remux_home().join("bin").join("remux")
}

fn staged_dir() -> PathBuf {
    crate::config::remux_home().join("staged")
}

fn staged_binary_path() -> PathBuf {
    staged_dir().join("remux")
}

fn last_check_path() -> PathBuf {
    crate::config::remux_home().join("last_update_check")
}

// --- Swap-on-boot ---

/// Check for a staged update and apply it. Runs synchronously on startup.
/// On any error, logs and cleans up — never prevents boot.
pub fn apply_staged_update() {
    let staged = staged_binary_path();
    if !staged.exists() {
        return;
    }

    if !is_valid_binary(&staged) {
        eprintln!("remux: staged update binary is invalid, cleaning up");
        cleanup_staged();
        return;
    }

    let target = bin_path();
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!("remux: failed to create bin directory: {}", e);
            cleanup_staged();
            return;
        }
    }

    match std::fs::rename(&staged, &target) {
        Ok(()) => {
            eprintln!("remux: updated successfully");
            cleanup_staged();
        }
        Err(e) => {
            eprintln!("remux: failed to apply update: {}", e);
            cleanup_staged();
        }
    }
}

fn cleanup_staged() {
    let dir = staged_dir();
    if dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            warn!("Failed to clean up staged directory: {}", e);
        }
    }
}

fn is_valid_binary(path: &Path) -> bool {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };

    if !meta.is_file() || meta.len() < 1024 {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }

    true
}

// --- Background update check ---

/// Check for updates in the background. Never returns an error to the caller.
pub async fn background_update_check() {
    if let Err(e) = background_update_check_inner().await {
        debug!("Update check: {}", e);
    }
}

async fn background_update_check_inner() -> anyhow::Result<()> {
    if !should_check_for_update() {
        debug!("Skipping update check (checked recently)");
        return Ok(());
    }

    let latest = fetch_latest_version().await?;
    write_update_timestamp();

    if !is_newer(&latest, CURRENT_VERSION) {
        debug!("Already up to date (current={}, latest={})", CURRENT_VERSION, latest);
        return Ok(());
    }

    info!("New version available: {} (current: {})", latest, CURRENT_VERSION);
    download_and_stage(&latest).await?;

    Ok(())
}

fn should_check_for_update() -> bool {
    let path = last_check_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return true,
    };

    let timestamp: u64 = match contents.trim().parse() {
        Ok(t) => t,
        Err(_) => return true,
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    now.saturating_sub(timestamp) >= CHECK_INTERVAL_SECS
}

fn write_update_timestamp() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let path = last_check_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, now.to_string()) {
        debug!("Failed to write update timestamp: {}", e);
    }
}

// --- GitHub API ---

#[derive(serde::Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

async fn fetch_latest_version() -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(format!("remux-cli/{}", CURRENT_VERSION))
        .build()?;

    let bytes = client
        .get(GITHUB_RELEASES_URL)
        .send()
        .await
        .context("Failed to check for updates")?
        .error_for_status()
        .context("GitHub API error")?
        .bytes()
        .await
        .context("Failed to read release info")?;

    let release: GitHubRelease =
        serde_json::from_slice(&bytes).context("Failed to parse release info")?;

    Ok(release.tag_name.trim_start_matches('v').to_string())
}

// --- Version comparison ---

fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

// --- Download and stage ---

fn platform_target() -> anyhow::Result<&'static str> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        (os, arch) => anyhow::bail!("Unsupported platform: {}/{}", os, arch),
    };
    Ok(target)
}

fn download_url(version: &str) -> anyhow::Result<String> {
    let target = platform_target()?;
    Ok(format!(
        "https://github.com/shivships/remux-cli/releases/download/v{}/remux-cli-{}.tar.gz",
        version, target
    ))
}

async fn download_and_stage(version: &str) -> anyhow::Result<()> {
    let url = download_url(version)?;
    let staged = staged_binary_path();

    let dir = staged_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create staged directory {}", dir.display()))?;

    debug!("Downloading update from {}", url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .user_agent(format!("remux-cli/{}", CURRENT_VERSION))
        .build()?;

    let bytes = client
        .get(&url)
        .send()
        .await
        .context("Failed to download update")?
        .error_for_status()
        .context("Download failed")?
        .bytes()
        .await
        .context("Failed to read update")?;

    // Extract binary from tarball (same pattern as cloudflared.rs)
    extract_binary_from_tarball(&bytes, &staged)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .context("Failed to set permissions on staged binary")?;
    }

    if !is_valid_binary(&staged) {
        cleanup_staged();
        anyhow::bail!("Staged binary failed validation");
    }

    info!("Staged update v{}", version);
    Ok(())
}

fn extract_binary_from_tarball(bytes: &[u8], target: &Path) -> anyhow::Result<()> {
    use flate2::read::GzDecoder;
    use std::io::Cursor;
    use tar::Archive;

    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = Archive::new(decoder);

    for entry in archive.entries().context("Failed to read tarball")? {
        let mut entry = entry.context("Failed to read tarball entry")?;
        let path = entry.path().context("Failed to read entry path")?;
        if path.file_name() == Some(std::ffi::OsStr::new("remux-cli")) {
            let mut file = std::fs::File::create(target)
                .with_context(|| format!("Failed to create {}", target.display()))?;
            std::io::copy(&mut entry, &mut file).context("Failed to extract binary")?;
            return Ok(());
        }
    }

    anyhow::bail!("Tarball does not contain 'remux-cli' binary")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_version("1.23.456"), Some((1, 23, 456)));
        assert_eq!(parse_version("bad"), None);
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn test_is_newer() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("bad", "0.1.0"));
    }
}
