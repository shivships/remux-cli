use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

const REPLAY_BUFFER_SIZE: usize = 64 * 1024;

pub struct SharedSession {
    state: std::sync::Mutex<SessionState>,
    replay: Arc<std::sync::Mutex<ReplayBuffer>>,
    broadcast_tx: broadcast::Sender<Vec<u8>>,
    pty_exited: watch::Sender<bool>,
    input_tx: mpsc::Sender<Vec<u8>>,
}

struct SessionState {
    pty: PtyState,
    clients: HashMap<u64, ClientInfo>,
    next_client_id: u64,
}

struct PtyState {
    // Declared BEFORE _child so master drops first: master drop → SIGHUP →
    // child exits → reader sees EOF. The _child handle drop is a no-op on
    // Unix (std::process::Child::drop doesn't reap), so ordering only matters
    // for the SIGHUP path.
    master: Box<dyn portable_pty::MasterPty + Send>,
    // Retained to keep the child handle alive for the session's lifetime and
    // preserve the option of explicit kill/wait in future graceful-shutdown
    // or re-spawn paths. Currently unused at runtime — termination flows
    // through the PTY SIGHUP path above.
    #[allow(dead_code)]
    _child: Box<dyn portable_pty::Child + Send + Sync>,
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
    pub fn new(
        shell: String,
        workspace_root: PathBuf,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Arc<Self>> {
        let (broadcast_tx, _) = broadcast::channel(256);
        let replay = Arc::new(std::sync::Mutex::new(ReplayBuffer::new()));
        let pty_exited = watch::Sender::new(false);

        // Capacity is in *events*, not bytes — one write_input call is one
        // user action (a key, a paste frame). 64 queued events is generous;
        // overflow parks the caller (cooperative backpressure) instead of
        // blocking the shared session state mutex.
        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>(64);

        // Fallible PTY construction first. Any `?` below returns before the
        // blocking tasks are spawned, so an init failure can't leak a thread.
        let pty_system = native_pty_system();
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pty_pair = pty_system.openpty(size)?;

        let mut cmd = CommandBuilder::new(&shell);
        if shell.ends_with("zsh") {
            cmd.args(["-o", "nopromptsp"]);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("REMUX_SESSION", "1");
        cmd.cwd(&workspace_root);

        let child = pty_pair.slave.spawn_command(cmd)?;
        drop(pty_pair.slave);

        let writer = pty_pair.master.take_writer()?;
        let reader = pty_pair.master.try_clone_reader()?;

        info!(cols, rows, "Terminal spawned");

        // Reader side.
        let replay_reader = Arc::clone(&replay);
        let broadcast_tx_reader = broadcast_tx.clone();
        let pty_exited_reader = pty_exited.clone();
        tokio::task::spawn_blocking(move || {
            Self::read_loop(reader, replay_reader, broadcast_tx_reader);
            let _ = pty_exited_reader.send(true);
        });

        // Writer side: dedicated task on the blocking thread pool.
        let pty_exited_writer = pty_exited.clone();
        tokio::task::spawn_blocking(move || {
            Self::write_loop(writer, input_rx);
            // Writer-side EOF (broken pipe, shell gone) also trips exit so
            // the session tears down even if the reader hasn't seen EOF yet.
            let _ = pty_exited_writer.send(true);
        });

        Ok(Arc::new(Self {
            state: std::sync::Mutex::new(SessionState {
                pty: PtyState {
                    master: pty_pair.master,
                    _child: child,
                },
                clients: HashMap::new(),
                next_client_id: 0,
            }),
            replay,
            broadcast_tx,
            pty_exited,
            input_tx,
        }))
    }

    pub async fn client_count(&self) -> usize {
        self.state.lock().unwrap().clients.len()
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
            let mut state = self.state.lock().unwrap();
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
        let mut state = self.state.lock().unwrap();
        state.clients.remove(&client_id);
        recalc_size(&state);
        info!(client_id, clients = state.clients.len(), "Client detached");
    }

    pub async fn resize(&self, client_id: u64, cols: u16, rows: u16) {
        let mut state = self.state.lock().unwrap();
        if let Some(client) = state.clients.get_mut(&client_id) {
            client.cols = cols;
            client.rows = rows;
        }
        recalc_size(&state);
    }

    pub async fn write_input(&self, data: Vec<u8>) {
        if let Err(e) = self.input_tx.send(data).await {
            warn!("Failed to queue PTY input: {}", e);
        }
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
    let min_cols = state.clients.values().map(|c| c.cols).min().unwrap();
    let min_rows = state.clients.values().map(|c| c.rows).min().unwrap();
    let size = PtySize {
        rows: min_rows,
        cols: min_cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    if let Err(e) = state.pty.master.resize(size) {
        warn!("Failed to resize PTY: {}", e);
    }
    debug!(cols = min_cols, rows = min_rows, "PTY resized");
}
