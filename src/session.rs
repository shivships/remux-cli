use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::protocol::{ResizePayload, MSG_TERMINAL_DATA, MSG_TERMINAL_RESIZE};
use crate::shared_session::SharedSession;

const HTTP_200: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\nok";

pub async fn run(stream: TcpStream, session: Arc<SharedSession>) {
    if let Err(e) = run_inner(stream, &session).await {
        warn!("Session error: {}", e);
    }
}

async fn run_inner(mut stream: TcpStream, session: &SharedSession) -> anyhow::Result<()> {
    let mut buf = [0u8; 4096];
    let n = stream.peek(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let lower = request.to_ascii_lowercase();

    if !lower.contains("upgrade: websocket") {
        debug!("Non-WebSocket HTTP request, responding with 200");
        stream.write_all(HTTP_200).await?;
        stream.shutdown().await?;
        return Ok(());
    }

    let ws_stream = accept_async(stream).await?;
    let (ws_sink, mut ws_source) = ws_stream.split();

    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Message>(256);

    let sink_task = tokio::spawn(async move {
        let mut sink = ws_sink;
        while let Some(msg) = outgoing_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut client_id: Option<u64> = None;
    let mut relay_task: Option<tokio::task::JoinHandle<()>> = None;

    while let Some(msg) = ws_source.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                debug!("WebSocket receive error: {}", e);
                break;
            }
        };

        match msg {
            Message::Binary(data) => {
                if data.is_empty() {
                    continue;
                }
                let msg_type = data[0];
                let payload = &data[1..];

                match msg_type {
                    MSG_TERMINAL_RESIZE => {
                        if let Ok(resize) = serde_json::from_slice::<ResizePayload>(payload) {
                            match client_id {
                                None => {
                                    match session.attach(resize.cols, resize.rows).await {
                                        Ok((id, replay_data, broadcast_rx)) => {
                                            client_id = Some(id);

                                            if !replay_data.is_empty() {
                                                let mut frame =
                                                    Vec::with_capacity(1 + replay_data.len());
                                                frame.push(MSG_TERMINAL_DATA);
                                                frame.extend_from_slice(&replay_data);
                                                let _ = outgoing_tx
                                                    .send(Message::Binary(frame.into()))
                                                    .await;
                                            }

                                            relay_task = Some(tokio::spawn(relay_broadcast(
                                                broadcast_rx,
                                                outgoing_tx.clone(),
                                            )));
                                        }
                                        Err(e) => {
                                            warn!("Failed to attach: {}", e);
                                        }
                                    }
                                }
                                Some(id) => {
                                    session.resize(id, resize.cols, resize.rows).await;
                                }
                            }
                        }
                    }
                    MSG_TERMINAL_DATA => {
                        if client_id.is_some() {
                            session.write_input(payload).await;
                        }
                    }
                    _ => {
                        debug!(msg_type, "Unknown binary message type");
                    }
                }
            }
            Message::Ping(data) => {
                let _ = outgoing_tx.send(Message::Pong(data)).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    if let Some(id) = client_id {
        session.detach(id).await;
    }
    if let Some(task) = relay_task {
        task.abort();
    }
    drop(outgoing_tx);
    let _ = sink_task.await;

    info!("Session ended");
    Ok(())
}

async fn relay_broadcast(
    mut broadcast_rx: broadcast::Receiver<Vec<u8>>,
    outgoing_tx: mpsc::Sender<Message>,
) {
    loop {
        match broadcast_rx.recv().await {
            Ok(data) => {
                let mut frame = Vec::with_capacity(1 + data.len());
                frame.push(MSG_TERMINAL_DATA);
                frame.extend_from_slice(&data);
                if outgoing_tx.send(Message::Binary(frame.into())).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(n, "Client lagged, missed broadcast messages");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
