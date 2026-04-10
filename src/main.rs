use std::io::Write;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

mod cloudflared;
mod config;
mod local;
mod modal;
mod protocol;
mod server;
mod session;
mod shared_session;
mod tunnel;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("remux_cli=error")),
        )
        .with_writer(std::io::stderr)
        .init();

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "zsh".into());
    let workspace_root = std::env::current_dir()?;
    let (cols, rows) = crossterm::terminal::size()?;
    // Reserve the bottom row for the status bar.
    let pty_rows = rows.saturating_sub(1).max(1);

    let config = config::Config::load();

    // Note: this spawns the PTY (and therefore the shell's rc files) before
    // the tunnel is started below. Any rc-file output lands in the replay
    // buffer and is delivered to the first client on attach.
    let session = shared_session::SharedSession::new(shell, workspace_root, cols, pty_rows)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    // Resolve cloudflared binary (may prompt user to download, exits on decline)
    let cloudflared_bin = match cloudflared::ensure_cloudflared().await {
        Ok(path) => path,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    // Start tunnel with spinner animation (pre-alt-screen)
    let mut tunnel_child = None;
    let mut full_url: Option<String> = None;
    let mut display_url: Option<String> = None;

    {
        const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let mut stdout = std::io::stdout();
        let mut frame = 0usize;

        write!(stdout, "  {} Creating tunnel...", SPINNER[0])?;
        stdout.flush()?;

        let tunnel_fut = tunnel::spawn_tunnel(&cloudflared_bin, port);
        tokio::pin!(tunnel_fut);
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(80));

        let result = loop {
            tokio::select! {
                result = &mut tunnel_fut => {
                    // Clear spinner line before entering alt screen
                    write!(stdout, "\r\x1b[2K")?;
                    stdout.flush()?;
                    break result;
                }
                _ = interval.tick() => {
                    frame = (frame + 1) % SPINNER.len();
                    write!(stdout, "\r  {} Creating tunnel...", SPINNER[frame])?;
                    stdout.flush()?;
                }
            }
        };

        match result {
            Ok((child, tunnel_url)) => {
                let slug = tunnel_url
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .to_string();
                full_url = Some(format!("{}/{}", config.base_url, slug));
                display_url = Some(format!("{}/{}", config.display_base_url(), slug));
                tunnel::spawn_keepalive(tunnel_url);
                tunnel_child = Some(child);
            }
            Err(e) => {
                eprintln!("tunnel unavailable: {}", e);
            }
        }
    }

    // Restore terminal on panic
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = std::io::stdout().write_all(b"\x1b[?1049l\x1b[?25h");
        let _ = std::io::stdout().flush();
        default_hook(info);
    }));

    let _raw_guard = local::RawModeGuard::enter()?;

    tokio::select! {
        _ = local::run_local(session.clone(), full_url, display_url, config) => {}
        _ = server::accept_loop(listener, session.clone()) => {}
        _ = session.wait_for_exit() => {}
    }

    drop(_raw_guard);
    // Kill the tunnel subprocess before exiting. start_kill() is synchronous
    // and sends SIGKILL immediately; no need to await.
    if let Some(ref mut child) = tunnel_child {
        child.start_kill().ok();
    }
    // Background tokio tasks (SIGWINCH handler, keepalive, etc.) have no
    // cancellation mechanism and would prevent clean runtime shutdown.
    std::process::exit(0);
}
