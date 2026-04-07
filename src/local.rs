use std::fs::OpenOptions;
use std::io::{Read, Write as IoWrite};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, StdSyncHandler};
use alacritty_terminal::Term;
use tokio::sync::{broadcast, mpsc};

use crate::modal::ModalContent;
use crate::shared_session::SharedSession;

// --- Event proxy: forwards PtyWrite responses back to the PTY ---

#[derive(Clone)]
struct Proxy {
    pty_tx: mpsc::UnboundedSender<String>,
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            let _ = self.pty_tx.send(text);
        }
    }
}

// --- Terminal size for alacritty_terminal ---

struct TermSize {
    lines: usize,
    cols: usize,
}

impl TermSize {
    fn new(lines: u16, cols: u16) -> Self {
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
        // Alternate screen + clear + enable SGR mouse reporting with drag tracking
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
        // Disable mouse reporting + leave alternate screen + show cursor
        let _ = stdout.write_all(b"\x1b[?1002l\x1b[?1006l\x1b[?1049l\x1b[?25h");
        let _ = stdout.flush();
        std::thread::sleep(Duration::from_millis(50));
        unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
    }
}

// --- Mouse event parsing (SGR format) ---

enum StdinEvent {
    Data(Vec<u8>),                          // regular input → forward to PTY
    ScrollUp(i32),                          // scroll up N lines
    ScrollDown(i32),                        // scroll down N lines
    Mouse(Vec<u8>),                         // mouse event → forward to PTY (when inner program wants mouse)
    SelectStart { col: u16, row: u16, alt: bool },  // left click
    SelectUpdate(u16, u16),                 // drag to (col, row)
    SelectEnd,                              // mouse release — copy selection
    ModalToggle,                            // Ctrl+Q
    ModalCopy,                              // 'c' while modal open
    ModalQuit,                              // 'q' while modal open
    ModalDismiss,                           // esc while modal open
}

/// Parse stdin bytes, extracting scroll events from SGR mouse sequences.
/// SGR mouse format: \x1b[<button;col;row[Mm]
/// Button 64 = scroll up, 65 = scroll down.
fn parse_stdin(bytes: &[u8], inner_wants_mouse: bool, modal_open: bool) -> Vec<StdinEvent> {
    let mut events = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        // Ctrl+Q always toggles modal
        if bytes[i] == 0x11 {
            events.push(StdinEvent::ModalToggle);
            i += 1;
            continue;
        }

        // When modal is open, intercept keys
        if modal_open {
            match bytes[i] {
                b'c' | b'C' => { events.push(StdinEvent::ModalCopy); i += 1; continue; }
                b'q' | b'Q' => { events.push(StdinEvent::ModalQuit); i += 1; continue; }
                0x1b => {
                    // Bare Esc (not followed by [) dismisses modal
                    if i + 1 >= bytes.len() || bytes[i + 1] != b'[' {
                        events.push(StdinEvent::ModalDismiss);
                        i += 1;
                        continue;
                    }
                    // Escape sequence (mouse events, etc.) — skip silently
                    i += 2;
                    while i < bytes.len() && !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'~') {
                        i += 1;
                    }
                    if i < bytes.len() { i += 1; }
                    continue;
                }
                _ => { i += 1; continue; } // ignore other keys while modal open
            }
        }

        // Check for escape sequences
        if bytes[i] == 0x1b {
            let start = i;

            // SGR mouse: \x1b[<button;col;row[Mm]
            if i + 2 < bytes.len() && bytes[i + 1] == b'[' && bytes[i + 2] == b'<' {
                i += 3; // skip \x1b[<
                let seq_start = i;
                while i < bytes.len() && bytes[i] != b'M' && bytes[i] != b'm' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // consume M/m
                    let params = &bytes[seq_start..i - 1];
                    if let Ok(params_str) = std::str::from_utf8(params) {
                        let parts: Vec<&str> = params_str.split(';').collect();
                        if parts.len() >= 3 {
                            let button = parts[0].parse::<u32>().unwrap_or(999);
                            let col = parts[1].parse::<u16>().unwrap_or(1).saturating_sub(1); // 1-indexed → 0-indexed
                            let row = parts[2].parse::<u16>().unwrap_or(1).saturating_sub(1);
                            let is_release = bytes[i - 1] == b'm';

                            if inner_wants_mouse {
                                events.push(StdinEvent::Mouse(bytes[start..i].to_vec()));
                                continue;
                            }

                            // Button encoding: bits 2-3 = modifiers (4=shift, 8=alt, 16=ctrl)
                            let has_alt = button & 8 != 0;
                            let base_button = button & !0b11100; // strip modifiers

                            match base_button {
                                64 => { events.push(StdinEvent::ScrollUp(3)); continue; }
                                65 => { events.push(StdinEvent::ScrollDown(3)); continue; }
                                0 if is_release => { events.push(StdinEvent::SelectEnd); continue; }
                                0 => { events.push(StdinEvent::SelectStart { col, row, alt: has_alt }); continue; }
                                32 => { events.push(StdinEvent::SelectUpdate(col, row)); continue; }
                                _ => { continue; }
                            }
                        }
                    }
                    continue; // malformed — drop
                } else {
                    break; // incomplete at end of buffer
                }
            }

            // Other escape sequences (arrow keys, focus, etc.) — forward as data
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // CSI sequence: \x1b[ ... terminated by an alpha char or ~
                i += 2; // skip \x1b[
                while i < bytes.len() && !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'~') {
                    i += 1;
                }
                if i < bytes.len() { i += 1; } // consume terminator
            } else if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                // OSC sequence: \x1b] ... terminated by BEL or ST
                i += 2;
                while i < bytes.len() && bytes[i] != 0x07 {
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2; break;
                    }
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == 0x07 { i += 1; }
            } else {
                // Short escape (e.g., \x1bO...) — consume ESC + next byte
                i += 1;
                if i < bytes.len() { i += 1; }
            }
            events.push(StdinEvent::Data(bytes[start..i].to_vec()));
            continue;
        }

        // Regular bytes — collect a run
        let start = i;
        while i < bytes.len() && bytes[i] != 0x1b && bytes[i] != 0x11 {
            i += 1;
        }
        if i > start {
            events.push(StdinEvent::Data(bytes[start..i].to_vec()));
        }
    }

    events
}

// --- Rendering helpers ---

fn write_color(buf: &mut Vec<u8>, color: Color, is_fg: bool) {
    use std::io::Write;
    match color {
        Color::Spec(rgb) => {
            let _ = write!(buf, "\x1b[{};2;{};{};{}m", if is_fg { 38 } else { 48 }, rgb.r, rgb.g, rgb.b);
        }
        Color::Indexed(idx) => {
            let _ = write!(buf, "\x1b[{};5;{}m", if is_fg { 38 } else { 48 }, idx);
        }
        Color::Named(name) => {
            let base = if is_fg { 30 } else { 40 };
            let code = match name {
                NamedColor::Black | NamedColor::DimBlack => base,
                NamedColor::Red | NamedColor::DimRed => base + 1,
                NamedColor::Green | NamedColor::DimGreen => base + 2,
                NamedColor::Yellow | NamedColor::DimYellow => base + 3,
                NamedColor::Blue | NamedColor::DimBlue => base + 4,
                NamedColor::Magenta | NamedColor::DimMagenta => base + 5,
                NamedColor::Cyan | NamedColor::DimCyan => base + 6,
                NamedColor::White | NamedColor::DimWhite => base + 7,
                NamedColor::BrightBlack => base + 60,
                NamedColor::BrightRed => base + 61,
                NamedColor::BrightGreen => base + 62,
                NamedColor::BrightYellow => base + 63,
                NamedColor::BrightBlue => base + 64,
                NamedColor::BrightMagenta => base + 65,
                NamedColor::BrightCyan => base + 66,
                NamedColor::BrightWhite => base + 67,
                // Foreground/Background/Cursor → default
                _ => if is_fg { 39 } else { 49 },
            };
            let _ = write!(buf, "\x1b[{}m", code);
        }
    }
}

fn write_flags(buf: &mut Vec<u8>, flags: Flags) {
    if flags.contains(Flags::BOLD) {
        buf.extend_from_slice(b"\x1b[1m");
    }
    if flags.contains(Flags::DIM) {
        buf.extend_from_slice(b"\x1b[2m");
    }
    if flags.contains(Flags::ITALIC) {
        buf.extend_from_slice(b"\x1b[3m");
    }
    if flags.contains(Flags::UNDERLINE) {
        buf.extend_from_slice(b"\x1b[4m");
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        buf.extend_from_slice(b"\x1b[21m");
    } else if flags.contains(Flags::UNDERCURL) {
        buf.extend_from_slice(b"\x1b[4:3m");
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        buf.extend_from_slice(b"\x1b[4:4m");
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        buf.extend_from_slice(b"\x1b[4:5m");
    }
    if flags.contains(Flags::INVERSE) {
        buf.extend_from_slice(b"\x1b[7m");
    }
    if flags.contains(Flags::HIDDEN) {
        buf.extend_from_slice(b"\x1b[8m");
    }
    if flags.contains(Flags::STRIKEOUT) {
        buf.extend_from_slice(b"\x1b[9m");
    }
}

fn render_line(buf: &mut Vec<u8>, term: &Term<Proxy>, line: usize, left: usize, right: usize) {
    use std::io::Write;
    let _ = write!(buf, "\x1b[{};{}H", line + 1, left + 1);

    let grid = term.grid();
    let offset = grid.display_offset() as i32;
    let grid_line = Line(line as i32 - offset);
    let row = &grid[grid_line];

    // Get selection range for highlighting
    let sel_range = term.selection.as_ref().and_then(|s| s.to_range(term));

    // Reset at start, track state within line
    buf.extend_from_slice(b"\x1b[0m");
    let mut cur_fg = Color::Named(NamedColor::Foreground);
    let mut cur_bg = Color::Named(NamedColor::Background);
    let mut cur_flags = Flags::empty();
    let mut cur_selected = false;

    for col in left..=right {
        let cell = &row[Column(col)];

        if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
            || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }

        let point = Point::new(grid_line, Column(col));
        let selected = sel_range.as_ref().is_some_and(|r| r.contains(point));

        // Only emit SGR when attributes change
        let vis_flags = cell.flags & !(Flags::WRAPLINE | Flags::WIDE_CHAR);
        if cell.fg != cur_fg || cell.bg != cur_bg || vis_flags != cur_flags || selected != cur_selected {
            buf.extend_from_slice(b"\x1b[0m");
            if selected {
                buf.extend_from_slice(b"\x1b[7m"); // inverse for selection
            }
            if vis_flags != Flags::empty() {
                write_flags(buf, vis_flags);
            }
            if cell.fg != Color::Named(NamedColor::Foreground) {
                write_color(buf, cell.fg, true);
            }
            if cell.bg != Color::Named(NamedColor::Background) {
                write_color(buf, cell.bg, false);
            }
            cur_fg = cell.fg;
            cur_bg = cell.bg;
            cur_flags = vis_flags;
            cur_selected = selected;
        }

        let mut char_buf = [0u8; 4];
        buf.extend_from_slice(cell.c.encode_utf8(&mut char_buf).as_bytes());
    }

    buf.extend_from_slice(b"\x1b[0m");
}

fn render_full(buf: &mut Vec<u8>, term: &Term<Proxy>) {
    buf.extend_from_slice(b"\x1b[H\x1b[2J");
    for line in 0..term.screen_lines() {
        render_line(buf, term, line, 0, term.columns().saturating_sub(1));
    }
}

fn render_damage(buf: &mut Vec<u8>, term: &mut Term<Proxy>) {
    match term.damage() {
        TermDamage::Full => {
            render_full(buf, term);
        }
        TermDamage::Partial(iter) => {
            let damaged: Vec<_> = iter.collect();
            for dmg in damaged {
                render_line(buf, term, dmg.line, dmg.left, dmg.right);
            }
        }
    }
}

fn position_cursor(buf: &mut Vec<u8>, term: &Term<Proxy>) {
    use std::io::Write;
    let cursor = term.grid().cursor.point;
    let _ = write!(buf, "\x1b[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1);
}

fn draw_bar(stdout: &mut impl IoWrite, cols: u16, rows: u16, bar_url: Option<&str>, slug: Option<&str>, clients: usize) {
    let w = cols as usize;

    let left = if let (Some(display), Some(s)) = (bar_url, slug) {
        let full_url = format!("https://remux.sh/{}", s);
        format!(
            " \x1b]8;;{}\x07{}\x1b]8;;\x07 │ {} connected",
            full_url, display, clients
        )
    } else {
        format!(" {} connected", clients)
    };

    let left_visible = if let Some(display) = bar_url {
        format!(" {} │ {} connected", display, clients).len()
    } else {
        format!(" {} connected", clients).len()
    };

    let right = "Ctrl+Q: menu ";
    let right_seq = format!("{}", right);

    let gap = w.saturating_sub(left_visible + right.len());

    let _ = write!(
        stdout,
        "\x1b7\x1b[{};1H\x1b[48;2;180;189;104m\x1b[38;2;29;31;33m\x1b[2K",
        rows
    );
    if left_visible + right.len() <= w {
        let _ = write!(stdout, "{}{:gap$}{}", left, "", right_seq);
        let total = left_visible + gap + right.len();
        if total < w {
            let _ = write!(stdout, "{}", " ".repeat(w - total));
        }
    } else {
        let _ = write!(stdout, "{}", left);
        if left_visible < w {
            let _ = write!(stdout, "{}", " ".repeat(w - left_visible));
        }
    }
    let _ = write!(stdout, "\x1b[0m\x1b8");
}

// --- Base64 for OSC 52 clipboard ---

fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[((triple >> 6) & 0x3F) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(triple & 0x3F) as usize] as char } else { '=' });
    }
    out
}

// --- Mode sync: forward terminal mode changes to the outer terminal ---

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

// --- Debug logging ---

fn debug_log(msg: &str) {
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

    // Channel for terminal query responses (PtyWrite events)
    let (pty_write_tx, mut pty_write_rx) = mpsc::unbounded_channel::<String>();
    let proxy = Proxy { pty_tx: pty_write_tx };

    let mut term = Term::new(
        Config::default(),
        &TermSize::new(pty_rows, cols),
        proxy,
    );
    let mut parser = Processor::<StdSyncHandler>::new();
    let mut last_mode = *term.mode();
    debug_log(&format!("INIT mode: {}", format_mode(last_mode)));

    let mut stdout = std::io::stdout();

    // Set scroll region (reserve bottom row for bar)
    let _ = write!(stdout, "\x1b[1;{}r", pty_rows);

    // Feed replay and render initial state
    if !replay.is_empty() {
        parser.advance(&mut term, &replay);
    }
    let mut buf = Vec::new();
    render_full(&mut buf, &term);
    // Sync any modes set during replay
    let new_mode = *term.mode();
    sync_modes(&mut buf, last_mode, new_mode);
    last_mode = new_mode;
    term.reset_damage();
    position_cursor(&mut buf, &term);
    stdout.write_all(&buf)?;

    let count = session.client_count().await;
    draw_bar(&mut stdout, cols, rows, bar_url.as_deref(), slug.as_deref(), count);
    stdout.flush()?;

    // Modal state
    let modal_open = Arc::new(AtomicBool::new(false));
    let modal_content = slug.as_deref().map(|s| Arc::new(ModalContent::new(s)));

    // Stdin → parse mouse events + forward data
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

    // SIGWINCH → resize
    let resize_session = Arc::clone(&session);
    let resize_size = Arc::clone(&size);
    let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
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
                        parser.advance(&mut term, &data);

                        // Sync terminal modes (mouse, bracketed paste, etc.)
                        let new_mode = *term.mode();
                        if new_mode != last_mode {
                            debug_log(&format!("MODE {} -> {}", format_mode(last_mode), format_mode(new_mode)));
                            stdin_mode.store(new_mode.bits(), Ordering::Relaxed);
                        }

                        if modal_open.load(Ordering::SeqCst) {
                            // Modal is open — don't render grid, just sync modes
                            let mut buf = Vec::new();
                            sync_modes(&mut buf, last_mode, new_mode);
                            last_mode = new_mode;
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
                        let mut buf = Vec::new();
                        render_full(&mut buf, &term);
                        position_cursor(&mut buf, &term);
                        term.reset_damage();
                        stdout.write_all(&buf)?;
                        stdout.flush()?;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Handle stdin events (scroll, data, mouse)
            Some(event) = stdin_rx.recv() => {
                match event {
                    StdinEvent::Data(data) => {
                        // Clear selection and snap to bottom when user types
                        let needs_redraw = term.selection.is_some() || term.grid().display_offset() != 0;
                        term.selection = None;
                        if term.grid().display_offset() != 0 {
                            term.scroll_display(Scroll::Bottom);
                        }
                        if needs_redraw {
                            let mut buf = Vec::new();
                            render_full(&mut buf, &term);
                            position_cursor(&mut buf, &term);
                            term.reset_damage();
                            stdout.write_all(&buf)?;
                            let cols = size.cols.load(Ordering::Relaxed);
                            let rows = size.rows.load(Ordering::Relaxed);
                            let count = session.client_count().await;
                            draw_bar(&mut stdout, cols, rows, bar_url.as_deref(), slug.as_deref(), count);
                            stdout.flush()?;
                        }
                        session.write_input(&data).await;
                    }
                    StdinEvent::ScrollUp(n) => {
                        debug_log(&format!("SCROLL UP {} (offset before: {})", n, term.grid().display_offset()));
                        term.scroll_display(Scroll::Delta(n));
                        debug_log(&format!("  offset after: {}", term.grid().display_offset()));
                        let mut buf = Vec::new();
                        render_full(&mut buf, &term);
                        position_cursor(&mut buf, &term);
                        term.reset_damage();
                        stdout.write_all(&buf)?;
                        let cols = size.cols.load(Ordering::Relaxed);
                        let rows = size.rows.load(Ordering::Relaxed);
                        let count = session.client_count().await;
                        draw_bar(&mut stdout, cols, rows, bar_url.as_deref(), slug.as_deref(), count);
                        stdout.flush()?;
                    }
                    StdinEvent::ScrollDown(n) => {
                        debug_log(&format!("SCROLL DOWN {} (offset before: {})", n, term.grid().display_offset()));
                        term.scroll_display(Scroll::Delta(-n));
                        debug_log(&format!("  offset after: {}", term.grid().display_offset()));
                        let mut buf = Vec::new();
                        render_full(&mut buf, &term);
                        position_cursor(&mut buf, &term);
                        term.reset_damage();
                        stdout.write_all(&buf)?;
                        let cols = size.cols.load(Ordering::Relaxed);
                        let rows = size.rows.load(Ordering::Relaxed);
                        let count = session.client_count().await;
                        draw_bar(&mut stdout, cols, rows, bar_url.as_deref(), slug.as_deref(), count);
                        stdout.flush()?;
                    }
                    StdinEvent::Mouse(data) => {
                        // Inner program wants mouse — forward raw sequence to PTY
                        session.write_input(&data).await;
                    }
                    StdinEvent::SelectStart { col, row, alt } => {
                        let cols = size.cols.load(Ordering::Relaxed);
                        let rows = size.rows.load(Ordering::Relaxed);
                        let pty_rows = rows.saturating_sub(1).max(1);

                        // Click on status bar?
                        if row >= pty_rows {
                            let right_text = "Ctrl+Q: menu ";
                            let right_start = (cols as usize).saturating_sub(right_text.len());

                            if col as usize >= right_start {
                                // Clicked menu — open modal
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
                                // Clicked URL area — copy URL
                                let url = format!("https://remux.sh/{}", s);
                                let encoded = base64_encode(url.as_bytes());
                                let _ = write!(stdout, "\x1b]52;c;{}\x07", encoded);

                                // Flash "Copied!" over just the URL
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

                                let count = session.client_count().await;
                                draw_bar(&mut stdout, cols, rows, bar_url.as_deref(), slug.as_deref(), count);
                                stdout.flush()?;
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

                        // Determine selection type
                        let sel_type = if alt {
                            SelectionType::Block
                        } else {
                            match click_count {
                                2 => SelectionType::Semantic,
                                3 => SelectionType::Lines,
                                _ => SelectionType::Simple,
                            }
                        };

                        // Clear existing selection, start new one
                        term.selection = None;
                        let offset = term.grid().display_offset() as i32;
                        let point = Point::new(Line(row as i32 - offset), Column(col as usize));
                        let selection = Selection::new(sel_type, point, Side::Left);
                        term.selection = Some(selection);

                        // For double/triple click, also update the end point to trigger
                        // word/line expansion immediately
                        if click_count >= 2 {
                            if let Some(ref mut sel) = term.selection {
                                sel.update(point, Side::Right);
                            }
                        }

                        let mut buf = Vec::new();
                        render_full(&mut buf, &term);
                        position_cursor(&mut buf, &term);
                        term.reset_damage();
                        stdout.write_all(&buf)?;
                        let count = session.client_count().await;
                        draw_bar(&mut stdout, cols, rows, bar_url.as_deref(), slug.as_deref(), count);
                        stdout.flush()?;
                    }
                    StdinEvent::SelectUpdate(col, row) => {
                        if term.selection.is_some() {
                            let offset = term.grid().display_offset() as i32;
                            let point = Point::new(Line(row as i32 - offset), Column(col as usize));
                            if let Some(ref mut sel) = term.selection {
                                sel.update(point, Side::Right);
                            }

                            let mut buf = Vec::new();
                            render_full(&mut buf, &term);
                            position_cursor(&mut buf, &term);
                            term.reset_damage();
                            stdout.write_all(&buf)?;
                            stdout.flush()?;
                        }
                    }
                    StdinEvent::SelectEnd => {
                        // Auto-copy on release, keep selection visible
                        if let Some(text) = term.selection_to_string() {
                            if !text.is_empty() {
                                let encoded = base64_encode(text.as_bytes());
                                let _ = write!(stdout, "\x1b]52;c;{}\x07", encoded);
                                stdout.flush()?;
                            }
                        }
                        // Selection stays visible until next click or keystroke
                    }
                    StdinEvent::ModalToggle => {
                        let is_open = modal_open.load(Ordering::SeqCst);
                        if is_open {
                            // Close modal — redraw terminal
                            modal_open.store(false, Ordering::SeqCst);
                            let cols = size.cols.load(Ordering::Relaxed);
                            let rows = size.rows.load(Ordering::Relaxed);
                            let pty_rows = rows.saturating_sub(1).max(1);
                            let _ = write!(stdout, "\x1b[1;{}r\x1b[H\x1b[2J", pty_rows);
                            let mut buf = Vec::new();
                            render_full(&mut buf, &term);
                            position_cursor(&mut buf, &term);
                            term.reset_damage();
                            stdout.write_all(&buf)?;
                            let _ = stdout.write_all(b"\x1b[?25h"); // show cursor
                            let count = session.client_count().await;
                            draw_bar(&mut stdout, cols, rows, bar_url.as_deref(), slug.as_deref(), count);
                            stdout.flush()?;
                        } else if let Some(ref content) = modal_content {
                            // Open modal with animation
                            modal_open.store(true, Ordering::SeqCst);
                            let cols = size.cols.load(Ordering::Relaxed);
                            let rows = size.rows.load(Ordering::Relaxed);
                            let _ = stdout.write_all(b"\x1b[?25l"); // hide cursor
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
                            let cols = size.cols.load(Ordering::Relaxed);
                            let rows = size.rows.load(Ordering::Relaxed);
                            let pty_rows = rows.saturating_sub(1).max(1);
                            let _ = write!(stdout, "\x1b[1;{}r\x1b[H\x1b[2J", pty_rows);
                            let mut buf = Vec::new();
                            render_full(&mut buf, &term);
                            position_cursor(&mut buf, &term);
                            term.reset_damage();
                            stdout.write_all(&buf)?;
                            let _ = stdout.write_all(b"\x1b[?25h");
                            let count = session.client_count().await;
                            draw_bar(&mut stdout, cols, rows, bar_url.as_deref(), slug.as_deref(), count);
                            stdout.flush()?;
                        }
                    }
                }
            }
            // Forward terminal query responses back to the PTY
            Some(text) = pty_write_rx.recv() => {
                session.write_input(text.as_bytes()).await;
            }
            Some((cols, rows)) = resize_rx.recv() => {
                let pty_rows = rows.saturating_sub(1).max(1);
                term.resize(TermSize::new(pty_rows, cols));
                let _ = write!(stdout, "\x1b[1;{}r", pty_rows);

                let mut buf = Vec::new();
                render_full(&mut buf, &term);
                position_cursor(&mut buf, &term);
                term.reset_damage();
                stdout.write_all(&buf)?;

                let count = session.client_count().await;
                draw_bar(&mut stdout, cols, rows, bar_url.as_deref(), slug.as_deref(), count);
                stdout.flush()?;
            }
            _ = &mut stdin_task => break,
        }
    }

    session.detach(client_id).await;
    Ok(())
}
