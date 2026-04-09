use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

const REPLAY_BUFFER_SIZE: usize = 64 * 1024;

pub struct SharedSession {
    state: tokio::sync::Mutex<SessionState>,
    replay: Arc<std::sync::Mutex<ReplayBuffer>>,
    broadcast_tx: broadcast::Sender<Vec<u8>>,
    pty_exited: watch::Sender<bool>,
    input_tx: std::sync::OnceLock<mpsc::Sender<Vec<u8>>>,
    shell: String,
    workspace_root: PathBuf,
}

struct SessionState {
    pty: Option<PtyState>,
    clients: HashMap<u64, ClientInfo>,
    next_client_id: u64,
}

struct PtyState {
    master: Box<dyn portable_pty::MasterPty + Send>,
}

struct ClientInfo {
    cols: u16,
    rows: u16,
}

struct ReplayBuffer {
    buf: Vec<u8>,
    write_pos: usize,
    full: bool,
}

impl ReplayBuffer {
    fn new() -> Self {
        Self {
            buf: vec![0u8; REPLAY_BUFFER_SIZE],
            write_pos: 0,
            full: false,
        }
    }

    fn append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        if data.len() >= REPLAY_BUFFER_SIZE {
            let start = data.len() - REPLAY_BUFFER_SIZE;
            self.buf.copy_from_slice(&data[start..]);
            self.write_pos = 0;
            self.full = true;
            return;
        }

        let remaining = REPLAY_BUFFER_SIZE - self.write_pos;
        if data.len() <= remaining {
            self.buf[self.write_pos..self.write_pos + data.len()].copy_from_slice(data);
            self.write_pos += data.len();
            if self.write_pos == REPLAY_BUFFER_SIZE {
                self.write_pos = 0;
                self.full = true;
            }
        } else {
            self.buf[self.write_pos..].copy_from_slice(&data[..remaining]);
            let leftover = data.len() - remaining;
            self.buf[..leftover].copy_from_slice(&data[remaining..]);
            self.write_pos = leftover;
            self.full = true;
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        if self.full {
            let mut out = Vec::with_capacity(REPLAY_BUFFER_SIZE);
            out.extend_from_slice(&self.buf[self.write_pos..]);
            out.extend_from_slice(&self.buf[..self.write_pos]);
            out
        } else {
            self.buf[..self.write_pos].to_vec()
        }
    }
}

impl SharedSession {
    pub fn new(shell: String, workspace_root: PathBuf) -> Arc<Self> {
        let (broadcast_tx, _) = broadcast::channel(256);
        Arc::new(Self {
            state: tokio::sync::Mutex::new(SessionState {
                pty: None,
                clients: HashMap::new(),
                next_client_id: 0,
            }),
            replay: Arc::new(std::sync::Mutex::new(ReplayBuffer::new())),
            broadcast_tx,
            pty_exited: watch::Sender::new(false),
            input_tx: std::sync::OnceLock::new(),
            shell,
            workspace_root,
        })
    }

    pub async fn client_count(&self) -> usize {
        self.state.lock().await.clients.len()
    }

    pub async fn spawn(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        let mut state = self.state.lock().await;
        if state.pty.is_none() {
            let pty = self.spawn_pty(cols, rows)?;
            info!(cols, rows, "Terminal spawned");
            state.pty = Some(pty);
        }
        Ok(())
    }

    pub async fn wait_for_exit(&self) {
        let mut rx = self.pty_exited.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    pub async fn attach(
        &self,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<(u64, Vec<u8>, broadcast::Receiver<Vec<u8>>)> {
        let client_id = {
            let mut state = self.state.lock().await;
            let id = state.next_client_id;
            state.next_client_id += 1;
            state.clients.insert(id, ClientInfo { cols, rows });
            recalc_size(&state);
            id
        };

        let (replay_data, rx) = {
            let replay = self.replay.lock().unwrap();
            let rx = self.broadcast_tx.subscribe();
            let data = replay.snapshot();
            (data, rx)
        };

        info!(client_id, replay_bytes = replay_data.len(), "Client attached");
        Ok((client_id, replay_data, rx))
    }

    pub async fn detach(&self, client_id: u64) {
        let mut state = self.state.lock().await;
        state.clients.remove(&client_id);
        recalc_size(&state);
        info!(client_id, clients = state.clients.len(), "Client detached");
    }

    pub async fn resize(&self, client_id: u64, cols: u16, rows: u16) {
        let mut state = self.state.lock().await;
        if let Some(client) = state.clients.get_mut(&client_id) {
            client.cols = cols;
            client.rows = rows;
        }
        recalc_size(&state);
    }

    pub async fn write_input(&self, data: Vec<u8>) {
        let Some(tx) = self.input_tx.get() else {
            return; // PTY not spawned yet
        };
        if let Err(e) = tx.send(data).await {
            warn!("Failed to queue PTY input: {}", e);
        }
    }

    fn spawn_pty(&self, cols: u16, rows: u16) -> anyhow::Result<PtyState> {
        let pty_system = native_pty_system();
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pty_pair = pty_system.openpty(size)?;

        let mut cmd = CommandBuilder::new(&self.shell);
        if self.shell.ends_with("zsh") {
            cmd.args(["-o", "nopromptsp"]);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("REMUX_SESSION", "1");
        cmd.cwd(&self.workspace_root);

        let _child = pty_pair.slave.spawn_command(cmd)?;
        drop(pty_pair.slave);

        let writer = pty_pair.master.take_writer()?;
        let reader = pty_pair.master.try_clone_reader()?;

        // Reader side.
        let replay = Arc::clone(&self.replay);
        let broadcast_tx = self.broadcast_tx.clone();
        let pty_exited_reader = self.pty_exited.clone();
        tokio::task::spawn_blocking(move || {
            Self::read_loop(reader, replay, broadcast_tx);
            let _ = pty_exited_reader.send(true);
        });

        // Writer side: dedicated task on the blocking thread pool.
        // Capacity is in *events*, not bytes — one write_input call is one
        // user action (a key, a paste frame). 64 queued events is generous;
        // overflow parks the caller (cooperative backpressure) instead of
        // blocking the shared session state mutex.
        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>(64);

        // spawn() is guarded by `if state.pty.is_none()`, so this path runs
        // exactly once per SharedSession lifetime. A hard panic (rather than
        // debug_assert) ensures any future refactor that introduces a
        // re-spawn path fails loudly in release instead of silently leaving
        // a stale write_loop pointing at a replaced PTY master. The check
        // costs one atomic compare per session startup.
        self.input_tx
            .set(input_tx)
            .expect("spawn_pty called more than once");

        let pty_exited_writer = self.pty_exited.clone();
        tokio::task::spawn_blocking(move || {
            Self::write_loop(writer, input_rx);
            // Writer-side EOF (broken pipe, shell gone) also trips exit so
            // the session tears down even if the reader hasn't seen EOF yet.
            let _ = pty_exited_writer.send(true);
        });

        Ok(PtyState {
            master: pty_pair.master,
        })
    }

    fn read_loop(
        mut reader: Box<dyn Read + Send>,
        replay: Arc<std::sync::Mutex<ReplayBuffer>>,
        broadcast_tx: broadcast::Sender<Vec<u8>>,
    ) {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    let mut replay = replay.lock().unwrap();
                    replay.append(&data);
                    let _ = broadcast_tx.send(data);
                    drop(replay);
                }
                Err(e) => {
                    debug!("PTY read ended: {}", e);
                    break;
                }
            }
        }
        info!("PTY read loop ended");
    }

    fn write_loop(
        mut writer: Box<dyn Write + Send>,
        mut input_rx: mpsc::Receiver<Vec<u8>>,
    ) {
        while let Some(data) = input_rx.blocking_recv() {
            if let Err(e) = writer.write_all(&data) {
                warn!("PTY write failed, ending write loop: {}", e);
                break;
            }
        }
        debug!("PTY write loop ended");
    }
}

fn recalc_size(state: &SessionState) {
    if state.clients.is_empty() {
        return;
    }
    if let Some(ref pty) = state.pty {
        let min_cols = state.clients.values().map(|c| c.cols).min().unwrap();
        let min_rows = state.clients.values().map(|c| c.rows).min().unwrap();
        let size = PtySize {
            rows: min_rows,
            cols: min_cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        if let Err(e) = pty.master.resize(size) {
            warn!("Failed to resize PTY: {}", e);
        }
        debug!(cols = min_cols, rows = min_rows, "PTY resized");
    }
}
