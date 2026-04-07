use std::io::Write;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

mod local;
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

    let session = shared_session::SharedSession::new(shell, workspace_root);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    // Start tunnel, extract slug for status bar
    let mut _tunnel_child = None;
    let mut bar_url: Option<String> = None;
    let mut slug: Option<String> = None;

    match tunnel::spawn_tunnel(port).await {
        Ok((child, url)) => {
            let s = url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('.')
                .next()
                .unwrap_or("")
                .to_string();
            bar_url = Some(format!("remux.sh/{}", s));
            slug = Some(s);
            tunnel::spawn_keepalive(url);
            _tunnel_child = Some(child);
        }
        Err(e) => {
            eprintln!("tunnel unavailable: {}", e);
        }
    }

    // Spawn PTY at actual terminal size (minus status bar row)
    let pty_rows = rows.saturating_sub(1).max(1);
    session.spawn(cols, pty_rows).await?;

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
        _ = local::run_local(session.clone(), bar_url, slug) => {}
        _ = server::accept_loop(listener, session.clone()) => {}
        _ = session.wait_for_exit() => {}
    }

    drop(_raw_guard);
    std::process::exit(0);
}
