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
mod uninstall;
mod update;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("uninstall") {
        return uninstall::run();
    }

    update::apply_staged_update();

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

    // Resolve cloudflared binary (may prompt user to download)
    let cloudflared_bin = cloudflared::ensure_cloudflared().await?;

    // Start tunnel with spinner animation (pre-alt-screen)
    let tunnel_state_rx;

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

        let (child, tunnel_url, stderr_handle) = result?;

        tunnel_state_rx = tunnel::TunnelManager::start(
            child,
            tunnel_url,
            stderr_handle,
            cloudflared_bin.clone(),
            port,
            config.base_url.clone(),
            config.display_base_url().to_string(),
        );
    }

    // Restore terminal on panic
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = std::io::stdout().write_all(b"\x1b[?1049l\x1b[?25h");
        let _ = std::io::stdout().flush();
        default_hook(info);
    }));

    // Background update check — fire and forget
    tokio::spawn(update::background_update_check());

    let _raw_guard = local::RawModeGuard::enter()?;

    tokio::select! {
        _ = local::run_local(session.clone(), tunnel_state_rx, config) => {}
        _ = server::accept_loop(listener, session.clone()) => {}
        _ = session.wait_for_exit() => {}
    }

    drop(_raw_guard);
    // Background tokio tasks (SIGWINCH handler, keepalive, etc.) have no
    // cancellation mechanism and would prevent clean runtime shutdown.
    std::process::exit(0);
}
