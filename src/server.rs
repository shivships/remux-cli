use std::sync::Arc;

use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::session;
use crate::shared_session::SharedSession;

pub async fn accept_loop(listener: TcpListener, session: Arc<SharedSession>) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                info!(%addr, "Client connected");
                let session = Arc::clone(&session);
                tokio::spawn(async move {
                    session::run(stream, session).await;
                    info!(%addr, "Client disconnected");
                });
            }
            Err(e) => {
                warn!("Failed to accept connection: {}", e);
            }
        }
    }
}
