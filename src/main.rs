use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

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

    let session = shared_session::SharedSession::new(shell, workspace_root);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let (_tunnel_child, url) = tunnel::spawn_tunnel(port).await?;
    tunnel::spawn_keepalive(url.clone());
    eprintln!("{}", url);

    session.spawn(80, 24).await?;

    tokio::select! {
        _ = server::accept_loop(listener, session.clone()) => {}
        _ = session.wait_for_exit() => {}
        _ = tokio::signal::ctrl_c() => {}
    }

    Ok(())
}
