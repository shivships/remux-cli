mod input;
mod render;

use std::fs::OpenOptions;
use std::io::{Read, Write as IoWrite};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle, Processor, StdSyncHandler};
use alacritty_terminal::Term;
use tokio::sync::{broadcast, mpsc};

use crate::modal::ModalContent;
use crate::shared_session::SharedSession;

use input::{parse_stdin, StdinEvent};
use render::{base64_encode, draw_bar, position_cursor, render_damage, render_full};

// --- Event proxy: forwards PtyWrite responses back to the PTY ---

#[derive(Clone)]
pub(crate) struct Proxy {
    pty_tx: mpsc::UnboundedSender<String>,
    pending_stdout: Arc<std::sync::Mutex<Vec<String>>>,
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => { let _ = self.pty_tx.send(text); }
            Event::Title(title) => {
                self.pending_stdout.lock().unwrap().push(format!("\x1b]2;{}\x07", title));
            }
            Event::ResetTitle => {
                self.pending_stdout.lock().unwrap().push("\x1b]2;\x07".to_string());
            }
            Event::ClipboardStore(ty, data) => {
                let param = match ty {
                    alacritty_terminal::term::ClipboardType::Clipboard => "c",
                    alacritty_terminal::term::ClipboardType::Selection => "p",
                };
                let encoded = render::base64_encode(data.as_bytes());
                self.pending_stdout.lock().unwrap().push(format!("\x1b]52;{};{}\x07", param, encoded));
            }
            _ => {}
        }
    }
}

/// Scan raw PTY output for OSC 7 sequences and return them for forwarding.
fn extract_osc7(data: &[u8]) -> Option<Vec<u8>> {
    // Look for \x1b]7;...\x07
    let mut i = 0;
    let mut results = Vec::new();
    while i < data.len() {
        if data[i] == 0x1b && i + 2 < data.len() && data[i + 1] == b']' && data[i + 2] == b'7' {
            let start = i;
            i += 3;
            while i < data.len() && data[i] != 0x07 {
                if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            if i < data.len() && data[i] == 0x07 {
                i += 1;
            }
            results.extend_from_slice(&data[start..i]);
        } else {
            i += 1;
        }
    }
    if results.is_empty() { None } else { Some(results) }
}

// --- Terminal size for alacritty_terminal ---

pub(crate) struct TermSize {
    lines: usize,
    cols: usize,
}

impl TermSize {
    pub fn new(lines: u16, cols: u16) -> Self {
        Self { lines: lines as usize, cols: cols as usize }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize { self.lines }
    fn screen_lines(&self) -> usize { self.lines }
    fn columns(&self) -> usize { self.cols }
}

// --- Raw mode guard ---

pub struct RawModeGuard;

impl RawModeGuard {
    pub fn enter() -> anyhow::Result<Self> {
        let mut stdout = std::io::stdout();
        stdout.write_all(b"\x1b[?1049h\x1b[H\x1b[2J\x1b[?1002h\x1b[?1006h")?;
        stdout.flush()?;
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(b"\x1b[?1002l\x1b[?1006l\x1b[?1049l\x1b[?25h");
        let _ = stdout.flush();
        std::thread::sleep(Duration::from_millis(50));
        unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
    }
}

// --- Mode sync ---

fn sync_modes(buf: &mut Vec<u8>, old: TermMode, new: TermMode) {
    sync_mode(buf, old, new, TermMode::SHOW_CURSOR, 25);
    sync_mode(buf, old, new, TermMode::APP_CURSOR, 1);
    sync_mode(buf, old, new, TermMode::MOUSE_REPORT_CLICK, 1000);
    sync_mode(buf, old, new, TermMode::MOUSE_DRAG, 1002);
    sync_mode(buf, old, new, TermMode::MOUSE_MOTION, 1003);
    sync_mode(buf, old, new, TermMode::UTF8_MOUSE, 1005);
    sync_mode(buf, old, new, TermMode::SGR_MOUSE, 1006);
    sync_mode(buf, old, new, TermMode::FOCUS_IN_OUT, 1004);
    sync_mode(buf, old, new, TermMode::BRACKETED_PASTE, 2004);
    sync_mode(buf, old, new, TermMode::ALTERNATE_SCROLL, 1007);
}

fn sync_mode(buf: &mut Vec<u8>, old: TermMode, new: TermMode, flag: TermMode, code: u16) {
    use std::io::Write;
    let was_set = old.contains(flag);
    let is_set = new.contains(flag);
    if was_set != is_set {
        let _ = write!(buf, "\x1b[?{}{}", code, if is_set { 'h' } else { 'l' });
    }
}

// --- Cursor shape sync ---

fn sync_cursor_style(buf: &mut Vec<u8>, old: CursorStyle, new: CursorStyle) {
    if old == new {
        return;
    }
    use std::io::Write;
    let code = match (new.shape, new.blinking) {
        (CursorShape::Block, true) => 1,
        (CursorShape::Block, false) => 2,
        (CursorShape::Underline, true) => 3,
        (CursorShape::Underline, false) => 4,
        (CursorShape::Beam, true) => 5,
        (CursorShape::Beam, false) => 6,
        _ => 0, // default
    };
    let _ = write!(buf, "\x1b[{} q", code);
}

// --- Debug logging ---

fn debug_log(msg: &str) {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    let enabled = ENABLED.get_or_init(|| std::env::var("REMUX_DEBUG").is_ok());
    if !enabled { return; }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open("/tmp/remux-debug.log") {
        let _ = writeln!(f, "{}", msg);
    }
}

fn format_mode(mode: TermMode) -> String {
    let mut flags = Vec::new();
    if mode.contains(TermMode::SHOW_CURSOR) { flags.push("SHOW_CURSOR"); }
    if mode.contains(TermMode::APP_CURSOR) { flags.push("APP_CURSOR"); }
    if mode.contains(TermMode::MOUSE_REPORT_CLICK) { flags.push("MOUSE_CLICK"); }
    if mode.contains(TermMode::MOUSE_DRAG) { flags.push("MOUSE_DRAG"); }
    if mode.contains(TermMode::MOUSE_MOTION) { flags.push("MOUSE_MOTION"); }
    if mode.contains(TermMode::SGR_MOUSE) { flags.push("SGR_MOUSE"); }
    if mode.contains(TermMode::UTF8_MOUSE) { flags.push("UTF8_MOUSE"); }
    if mode.contains(TermMode::BRACKETED_PASTE) { flags.push("BRACKETED_PASTE"); }
    if mode.contains(TermMode::FOCUS_IN_OUT) { flags.push("FOCUS_IN_OUT"); }
    if mode.contains(TermMode::ALT_SCREEN) { flags.push("ALT_SCREEN"); }
    if mode.contains(TermMode::ALTERNATE_SCROLL) { flags.push("ALT_SCROLL"); }
    if mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL) { flags.push("KITTY_KB"); }
    flags.join(" | ")
}

fn format_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        if b == 0x1b {
            out.push_str("ESC ");
        } else if b.is_ascii_graphic() || b == b' ' {
            out.push(b as char);
        } else {
            out.push_str(&format!("\\x{:02x} ", b));
        }
    }
    out
}

// --- Main local loop ---

struct AtomicSize {
    cols: AtomicU16,
    rows: AtomicU16,
}

fn flush_term(stdout: &mut impl IoWrite, term: &mut Term<Proxy>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    render_full(&mut buf, term);
    position_cursor(&mut buf, term);
    term.reset_damage();
    stdout.write_all(&buf)
}

async fn flush_bar(
    stdout: &mut impl IoWrite,
    size: &AtomicSize,
    session: &SharedSession,
    bar_url: Option<&str>,
    slug: Option<&str>,
) -> std::io::Result<()> {
    let cols = size.cols.load(Ordering::Relaxed);
    let rows = size.rows.load(Ordering::Relaxed);
    let count = session.client_count().await;
    draw_bar(stdout, cols, rows, bar_url, slug, count);
    stdout.flush()
}

async fn flush_term_with_bar(
    stdout: &mut impl IoWrite,
    term: &mut Term<Proxy>,
    size: &AtomicSize,
    session: &SharedSession,
    bar_url: Option<&str>,
    slug: Option<&str>,
) -> std::io::Result<()> {
    flush_term(stdout, term)?;
    flush_bar(stdout, size, session, bar_url, slug).await
}

pub async fn run_local(
    session: Arc<SharedSession>,
    bar_url: Option<String>,
    slug: Option<String>,
) -> anyhow::Result<()> {
    let (cols, rows) = crossterm::terminal::size()?;
    let pty_rows = rows.saturating_sub(1).max(1);

    let (client_id, replay, broadcast_rx) = session.attach(cols, pty_rows).await?;

    let size = Arc::new(AtomicSize {
        cols: AtomicU16::new(cols),
        rows: AtomicU16::new(rows),
    });

    let (pty_write_tx, mut pty_write_rx) = mpsc::unbounded_channel::<String>();
    let pending_stdout = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let proxy = Proxy { pty_tx: pty_write_tx, pending_stdout: Arc::clone(&pending_stdout) };

    let mut term = Term::new(
        Config::default(),
        &TermSize::new(pty_rows, cols),
        proxy,
    );
    let mut parser = Processor::<StdSyncHandler>::new();
    let mut last_mode = *term.mode();
    let mut last_cursor_style = term.cursor_style();
    debug_log(&format!("INIT mode: {}", format_mode(last_mode)));

    let mut stdout = std::io::stdout();

    let _ = write!(stdout, "\x1b[1;{}r", pty_rows);

    if !replay.is_empty() {
        parser.advance(&mut term, &replay);
    }
    let mut buf = Vec::new();
    render_full(&mut buf, &term);
    let new_mode = *term.mode();
    sync_modes(&mut buf, last_mode, new_mode);
    last_mode = new_mode;
    let new_cursor = term.cursor_style();
    sync_cursor_style(&mut buf, last_cursor_style, new_cursor);
    last_cursor_style = new_cursor;
    term.reset_damage();
    position_cursor(&mut buf, &term);
    stdout.write_all(&buf)?;

    let count = session.client_count().await;
    draw_bar(&mut stdout, cols, rows, bar_url.as_deref(), slug.as_deref(), count);
    stdout.flush()?;

    // Modal state
    let modal_open = Arc::new(AtomicBool::new(false));
    let modal_content = slug.as_deref().map(|s| Arc::new(ModalContent::new(s)));

    // Stdin task
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<StdinEvent>();
    let stdin_mode = Arc::new(std::sync::atomic::AtomicU32::new(last_mode.bits()));
    let stdin_mode_clone = Arc::clone(&stdin_mode);
    let stdin_modal = Arc::clone(&modal_open);
    let mut stdin_task = tokio::task::spawn_blocking(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let bytes = &buf[..n];
                    debug_log(&format!("STDIN [{}]: {}", n, format_bytes(bytes)));
                    let mode_bits = stdin_mode_clone.load(Ordering::Relaxed);
                    let mode = TermMode::from_bits_truncate(mode_bits);
                    let inner_wants_mouse = mode.intersects(TermMode::MOUSE_MODE);
                    let is_modal = stdin_modal.load(Ordering::SeqCst);
                    let events = parse_stdin(bytes, inner_wants_mouse, is_modal);
                    for event in events {
                        let _ = stdin_tx.send(event);
                    }
                }
                Err(_) => break,
            }
        }
    });

    // SIGWINCH handler
    let resize_session = Arc::clone(&session);
    let resize_size = Arc::clone(&size);
    let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16)>();
    tokio::spawn(async move {
        let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            .expect("failed to register SIGWINCH");
        loop {
            sig.recv().await;
            if let Ok((c, r)) = crossterm::terminal::size() {
                let pr = r.saturating_sub(1).max(1);
                resize_session.resize(client_id, c, pr).await;
                resize_size.cols.store(c, Ordering::Relaxed);
                resize_size.rows.store(r, Ordering::Relaxed);
                let _ = resize_tx.send((c, r));
            }
        }
    });

    // Click tracking for double/triple-click
    let mut last_click_time = std::time::Instant::now();
    let mut last_click_pos: (u16, u16) = (0, 0);
    let mut click_count: u8 = 0;
    const DOUBLE_CLICK_TIMEOUT: Duration = Duration::from_millis(300);

    // Output loop
    let mut broadcast_rx = broadcast_rx;
    loop {
        tokio::select! {
            result = broadcast_rx.recv() => {
                match result {
                    Ok(data) => {
                        // Forward OSC 7 (cwd) to Ghostty before parser consumes it
                        if let Some(osc7) = extract_osc7(&data) {
                            stdout.write_all(&osc7)?;
                        }

                        parser.advance(&mut term, &data);

                        // Flush pending events (title, clipboard) to Ghostty
                        {
                            let mut pending = pending_stdout.lock().unwrap();
                            for seq in pending.drain(..) {
                                stdout.write_all(seq.as_bytes())?;
                            }
                        }

                        let new_mode = *term.mode();
                        if new_mode != last_mode {
                            debug_log(&format!("MODE {} -> {}", format_mode(last_mode), format_mode(new_mode)));
                            stdin_mode.store(new_mode.bits(), Ordering::Relaxed);
                        }

                        let new_cursor = term.cursor_style();

                        if modal_open.load(Ordering::SeqCst) {
                            let mut buf = Vec::new();
                            sync_modes(&mut buf, last_mode, new_mode);
                            last_mode = new_mode;
                            sync_cursor_style(&mut buf, last_cursor_style, new_cursor);
                            last_cursor_style = new_cursor;
                            term.reset_damage();
                            if !buf.is_empty() {
                                stdout.write_all(&buf)?;
                                stdout.flush()?;
                            }
                        } else {
                            let mut buf = Vec::new();
                            render_damage(&mut buf, &mut term);
                            sync_modes(&mut buf, last_mode, new_mode);
                            last_mode = new_mode;
                            sync_cursor_style(&mut buf, last_cursor_style, new_cursor);
                            last_cursor_style = new_cursor;
                            position_cursor(&mut buf, &term);
                            term.reset_damage();
                            stdout.write_all(&buf)?;

                            let cols = size.cols.load(Ordering::Relaxed);
                            let rows = size.rows.load(Ordering::Relaxed);
                            let count = session.client_count().await;
                            draw_bar(&mut stdout, cols, rows, bar_url.as_deref(), slug.as_deref(), count);
                            stdout.flush()?;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        flush_term(&mut stdout, &mut term)?;
                        stdout.flush()?;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            Some(event) = stdin_rx.recv() => {
                match event {
                    StdinEvent::Data(data) => {
                        let needs_redraw = term.selection.is_some() || term.grid().display_offset() != 0;
                        term.selection = None;
                        if term.grid().display_offset() != 0 {
                            term.scroll_display(Scroll::Bottom);
                        }
                        if needs_redraw {
                            flush_term_with_bar(&mut stdout, &mut term, &size, &session, bar_url.as_deref(), slug.as_deref()).await?;
                        }
                        session.write_input(data).await;
                    }
                    StdinEvent::ScrollUp(n) => {
                        debug_log(&format!("SCROLL UP {} (offset before: {})", n, term.grid().display_offset()));
                        term.scroll_display(Scroll::Delta(n));
                        debug_log(&format!("  offset after: {}", term.grid().display_offset()));
                        flush_term_with_bar(&mut stdout, &mut term, &size, &session, bar_url.as_deref(), slug.as_deref()).await?;
                    }
                    StdinEvent::ScrollDown(n) => {
                        debug_log(&format!("SCROLL DOWN {} (offset before: {})", n, term.grid().display_offset()));
                        term.scroll_display(Scroll::Delta(-n));
                        debug_log(&format!("  offset after: {}", term.grid().display_offset()));
                        flush_term_with_bar(&mut stdout, &mut term, &size, &session, bar_url.as_deref(), slug.as_deref()).await?;
                    }
                    StdinEvent::Mouse(data) => {
                        session.write_input(data).await;
                    }
                    StdinEvent::Focus(data) => {
                        session.write_input(data).await;
                    }
                    StdinEvent::SelectStart { col, row, alt } => {
                        let cols = size.cols.load(Ordering::Relaxed);
                        let rows = size.rows.load(Ordering::Relaxed);
                        let pty_rows = rows.saturating_sub(1).max(1);

                        if row >= pty_rows {
                            let right_text = "Ctrl+Q: menu ";
                            let right_start = (cols as usize).saturating_sub(right_text.len());

                            if col as usize >= right_start {
                                if let Some(ref content) = modal_content {
                                    modal_open.store(true, Ordering::SeqCst);
                                    let _ = stdout.write_all(b"\x1b[?25l");
                                    for frame in 0..3u8 {
                                        let data = content.render_frame(cols, rows, frame);
                                        stdout.write_all(&data)?;
                                        stdout.flush()?;
                                        if frame < 2 {
                                            tokio::time::sleep(Duration::from_millis(33)).await;
                                        }
                                    }
                                }
                                continue;
                            } else if let Some(ref s) = slug {
                                let url = format!("https://remux.sh/{}", s);
                                let encoded = base64_encode(url.as_bytes());
                                let _ = write!(stdout, "\x1b]52;c;{}\x07", encoded);

                                let display = bar_url.as_deref().unwrap_or("");
                                let copied = "Copied!";
                                let pad = display.len().saturating_sub(copied.len());
                                let _ = write!(
                                    stdout,
                                    "\x1b7\x1b[{};2H\x1b[48;2;180;189;104m\x1b[1;38;2;29;31;33m{}{}\x1b[0m\x1b8",
                                    rows, copied, " ".repeat(pad)
                                );
                                stdout.flush()?;
                                tokio::time::sleep(Duration::from_millis(800)).await;
                                flush_bar(&mut stdout, &size, &session, bar_url.as_deref(), slug.as_deref()).await?;
                            }
                            continue;
                        }

                        // Track click count for double/triple-click
                        let now = std::time::Instant::now();
                        if now.duration_since(last_click_time) < DOUBLE_CLICK_TIMEOUT
                            && last_click_pos == (col, row)
                        {
                            click_count = (click_count % 3) + 1;
                        } else {
                            click_count = 1;
                        }
                        last_click_time = now;
                        last_click_pos = (col, row);

                        let sel_type = if alt {
                            SelectionType::Block
                        } else {
                            match click_count {
                                2 => SelectionType::Semantic,
                                3 => SelectionType::Lines,
                                _ => SelectionType::Simple,
                            }
                        };

                        term.selection = None;
                        let offset = term.grid().display_offset() as i32;
                        let point = Point::new(Line(row as i32 - offset), Column(col as usize));
                        let selection = Selection::new(sel_type, point, Side::Left);
                        term.selection = Some(selection);

                        if click_count >= 2 {
                            if let Some(ref mut sel) = term.selection {
                                sel.update(point, Side::Right);
                            }
                        }

                        flush_term_with_bar(&mut stdout, &mut term, &size, &session, bar_url.as_deref(), slug.as_deref()).await?;
                    }
                    StdinEvent::SelectUpdate(col, row) => {
                        if term.selection.is_some() {
                            let offset = term.grid().display_offset() as i32;
                            let point = Point::new(Line(row as i32 - offset), Column(col as usize));
                            if let Some(ref mut sel) = term.selection {
                                sel.update(point, Side::Right);
                            }

                            flush_term_with_bar(&mut stdout, &mut term, &size, &session, bar_url.as_deref(), slug.as_deref()).await?;
                        }
                    }
                    StdinEvent::SelectEnd => {
                        // Always copy on release
                        if let Some(text) = term.selection_to_string() {
                            if !text.is_empty() {
                                let encoded = base64_encode(text.as_bytes());
                                let _ = write!(stdout, "\x1b]52;c;{}\x07", encoded);
                            }
                        }
                        // Double/triple click: keep selection visible
                        // Single click drag: clear selection
                        if click_count <= 1 {
                            term.selection = None;
                            flush_term_with_bar(&mut stdout, &mut term, &size, &session, bar_url.as_deref(), slug.as_deref()).await?;
                        }
                        stdout.flush()?;
                    }
                    StdinEvent::ModalToggle => {
                        let is_open = modal_open.load(Ordering::SeqCst);
                        if is_open {
                            modal_open.store(false, Ordering::SeqCst);
                            let pty_rows = size.rows.load(Ordering::Relaxed).saturating_sub(1).max(1);
                            let _ = write!(stdout, "\x1b[1;{}r\x1b[H\x1b[2J", pty_rows);
                            flush_term(&mut stdout, &mut term)?;
                            let _ = stdout.write_all(b"\x1b[?25h");
                            flush_bar(&mut stdout, &size, &session, bar_url.as_deref(), slug.as_deref()).await?;
                        } else if let Some(ref content) = modal_content {
                            modal_open.store(true, Ordering::SeqCst);
                            let cols = size.cols.load(Ordering::Relaxed);
                            let rows = size.rows.load(Ordering::Relaxed);
                            let _ = stdout.write_all(b"\x1b[?25l");
                            for frame in 0..3u8 {
                                let data = content.render_frame(cols, rows, frame);
                                stdout.write_all(&data)?;
                                stdout.flush()?;
                                if frame < 2 {
                                    tokio::time::sleep(Duration::from_millis(33)).await;
                                }
                            }
                        }
                    }
                    StdinEvent::ModalCopy => {
                        if let Some(ref content) = modal_content {
                            let encoded = base64_encode(content.url.as_bytes());
                            let _ = write!(stdout, "\x1b]52;c;{}\x07", encoded);
                            let cols = size.cols.load(Ordering::Relaxed);
                            let rows = size.rows.load(Ordering::Relaxed);
                            let flash = content.render_copied_flash(cols, rows);
                            stdout.write_all(&flash)?;
                            stdout.flush()?;
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            let frame = content.render_full(cols, rows);
                            stdout.write_all(&frame)?;
                            stdout.flush()?;
                        }
                    }
                    StdinEvent::ModalQuit => {
                        break;
                    }
                    StdinEvent::ModalDismiss => {
                        if modal_open.load(Ordering::SeqCst) {
                            modal_open.store(false, Ordering::SeqCst);
                            let pty_rows = size.rows.load(Ordering::Relaxed).saturating_sub(1).max(1);
                            let _ = write!(stdout, "\x1b[1;{}r\x1b[H\x1b[2J", pty_rows);
                            flush_term(&mut stdout, &mut term)?;
                            let _ = stdout.write_all(b"\x1b[?25h");
                            flush_bar(&mut stdout, &size, &session, bar_url.as_deref(), slug.as_deref()).await?;
                        }
                    }
                }
            }
            Some(text) = pty_write_rx.recv() => {
                session.write_input(text.into_bytes()).await;
            }
            Some((cols, rows)) = resize_rx.recv() => {
                let pty_rows = rows.saturating_sub(1).max(1);
                term.resize(TermSize::new(pty_rows, cols));
                let _ = write!(stdout, "\x1b[1;{}r", pty_rows);
                flush_term_with_bar(&mut stdout, &mut term, &size, &session, bar_url.as_deref(), slug.as_deref()).await?;
            }
            _ = &mut stdin_task => break,
        }
    }

    session.detach(client_id).await;
    Ok(())
}
