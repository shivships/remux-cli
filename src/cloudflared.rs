use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use tracing::info;

/// Returns the absolute path to a usable cloudflared binary.
/// If none is found, prompts the user to download it.
///
/// Must be called before the terminal enters raw mode (needs stdin for prompt).
pub async fn ensure_cloudflared() -> anyhow::Result<PathBuf> {
    match resolve_cloudflared()? {
        Some(path) => {
            info!(path = %path.display(), "Found cloudflared");
            Ok(path)
        }
        None => {
            if prompt_user_download()? {
                download_and_install().await
            } else {
                anyhow::bail!(
                    "cloudflared is required for remux to work.\n\
                     Install manually: https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/"
                );
            }
        }
    }
}

/// Check all known locations for a cloudflared binary.
fn resolve_cloudflared() -> anyhow::Result<Option<PathBuf>> {
    // 1. Explicit override via env var
    if let Ok(explicit) = std::env::var("REMUX_CLOUDFLARED_BIN") {
        let path = PathBuf::from(&explicit);
        if is_executable(&path) {
            return Ok(Some(path));
        }
        anyhow::bail!(
            "REMUX_CLOUDFLARED_BIN is set to '{}' but no executable file exists at that path",
            explicit
        );
    }

    // 2. Search system PATH
    if let Some(path) = find_in_path() {
        return Ok(Some(path));
    }

    // 3. Check managed install location
    let managed = managed_bin_path();
    if is_executable(&managed) {
        return Ok(Some(managed));
    }

    Ok(None)
}

/// Search $PATH for cloudflared.
fn find_in_path() -> Option<PathBuf> {
    let bin_name = if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    };
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(bin_name))
            .find(|path| is_executable(path))
    })
}

/// Where we install cloudflared when the user opts in to auto-download.
fn managed_bin_path() -> PathBuf {
    let bin_name = if cfg!(windows) { "cloudflared.exe" } else { "cloudflared" };
    crate::config::remux_home().join("bin").join(bin_name)
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Prompt the user to download cloudflared. Returns true if they accept.
fn prompt_user_download() -> anyhow::Result<bool> {
    let size_hint = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", _) => "~38 MB",
        ("macos", _) => "~18 MB",
        ("windows", _) => "~25 MB",
        _ => "~30 MB",
    };
    let mut stdout = std::io::stdout();
    write!(
        stdout,
        "remux uses cloudflared to create a tunnel for remote access.\n\
         Download it now? ({}) [Y/n]: ",
        size_hint
    )?;
    stdout.flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_lowercase();

    Ok(trimmed.is_empty() || trimmed == "y" || trimmed == "yes")
}

/// Build the GitHub releases download URL for the current platform.
fn download_url() -> anyhow::Result<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    let artifact = match (os, arch) {
        ("linux", "x86_64") => "cloudflared-linux-amd64",
        ("linux", "aarch64") => "cloudflared-linux-arm64",
        ("macos", "x86_64") => "cloudflared-darwin-amd64.tgz",
        ("macos", "aarch64") => "cloudflared-darwin-arm64.tgz",
        ("windows", "x86_64") => "cloudflared-windows-amd64.exe",
        ("windows", "aarch64") => "cloudflared-windows-arm64.exe",
        _ => anyhow::bail!("Unsupported platform: {}/{}. Please install cloudflared manually.", os, arch),
    };

    Ok(format!(
        "https://github.com/cloudflare/cloudflared/releases/latest/download/{}",
        artifact
    ))
}

/// Download cloudflared and install it to the managed path.
async fn download_and_install() -> anyhow::Result<PathBuf> {
    use futures_util::StreamExt;

    let url = download_url()?;
    let target = managed_bin_path();

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }

    let response = reqwest::get(&url)
        .await
        .context("Failed to download cloudflared")?
        .error_for_status()
        .context("Failed to download cloudflared")?;

    let total = response.content_length();
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut bytes = Vec::new();

    let mut stderr = std::io::stderr();
    write!(stderr, "Downloading cloudflared... ")?;
    stderr.flush()?;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Failed to read cloudflared download")?;
        downloaded += chunk.len() as u64;
        bytes.extend_from_slice(&chunk);

        if let Some(total) = total {
            let pct = (downloaded as f64 / total as f64).min(1.0);
            let filled = (pct * 30.0) as usize;
            let empty = 30 - filled;
            write!(
                stderr,
                "\r\x1b[KDownloading cloudflared  [{}{}] {:3}%",
                "█".repeat(filled),
                "░".repeat(empty),
                (pct * 100.0) as u32,
            )?;
            stderr.flush()?;
        }
    }

    write!(stderr, "\r\x1b[KDownloading cloudflared  done.\n")?;
    stderr.flush()?;

    install_binary(&bytes, &target)?;

    eprintln!("Installed cloudflared to {}", target.display());
    Ok(target)
}

/// Write the binary to disk, handling platform-specific extraction and permissions.
fn install_binary(bytes: &[u8], target: &Path) -> anyhow::Result<()> {
    let os = std::env::consts::OS;

    if os == "macos" {
        // macOS artifacts are .tgz archives
        extract_tgz(bytes, target)?;
    } else {
        // Linux and Windows are bare binaries
        std::fs::write(target, bytes)
            .with_context(|| format!("Failed to write cloudflared to {}", target.display()))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("Failed to set permissions on {}", target.display()))?;
    }

    Ok(())
}

/// Extract the cloudflared binary from a .tgz archive (macOS).
fn extract_tgz(bytes: &[u8], target: &Path) -> anyhow::Result<()> {
    use flate2::read::GzDecoder;
    use std::io::Cursor;
    use tar::Archive;

    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = Archive::new(decoder);

    for entry in archive.entries().context("Failed to read tgz archive")? {
        let mut entry = entry.context("Failed to read tgz entry")?;
        let path = entry.path().context("Failed to read tgz entry path")?;
        if path.file_name() == Some(std::ffi::OsStr::new("cloudflared")) {
            let mut file = std::fs::File::create(target)
                .with_context(|| format!("Failed to create {}", target.display()))?;
            std::io::copy(&mut entry, &mut file)
                .context("Failed to extract cloudflared from archive")?;
            return Ok(());
        }
    }

    anyhow::bail!("Downloaded archive does not contain 'cloudflared' binary")
}
