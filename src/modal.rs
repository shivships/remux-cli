use std::fmt::Write as FmtWrite;

const MODAL_BG: &str = "\x1b[48;2;38;38;38m";          // #262626
const PRIMARY: &str = "\x1b[38;2;250;250;250m";        // #FAFAFA
const BOLD_PRIMARY: &str = "\x1b[1;38;2;250;250;250m"; // #FAFAFA bold — URL + "Copied!"
const SECONDARY: &str = "\x1b[38;2;163;163;163m";      // #A3A3A3 — title
const MUTED: &str = "\x1b[38;2;154;154;154m";          // #9A9A9A — hint verbs
const LIVE: &str = "\x1b[38;2;34;197;94m";             // #22C55E — toggle on
const RESET: &str = "\x1b[0m";

pub struct ModalContent {
    pub url: String,
    pub display_url: String,
    qr_lines: Vec<String>,
}

impl ModalContent {
    pub fn new(full_url: &str, display_url: &str) -> Self {
        let url = full_url.to_string();
        let display_url = display_url.to_string();

        let qr_lines = if let Ok(qr) = fast_qr::QRBuilder::new(url.as_str()).build() {
            qr.to_str().lines().map(|l| l.to_string()).collect()
        } else {
            Vec::new()
        };

        Self { url, display_url, qr_lines }
    }

    pub fn render_frame(&self, cols: u16, rows: u16, frame: u8, show_on_start: bool) -> Vec<u8> {
        let pty_rows = rows.saturating_sub(1).max(1) as usize;
        let all_lines = self.build_content_lines(cols, show_on_start);
        let box_height = all_lines.len();
        let box_width = all_lines.iter().map(|l| visible_len(l)).max().unwrap_or(0);

        if box_height == 0 || box_width == 0 || pty_rows < 3 {
            return Vec::new();
        }

        let visible_fraction = match frame {
            0 => 0.3,
            1 => 0.65,
            _ => 1.0,
        };
        let visible_count = ((box_height as f64) * visible_fraction).ceil() as usize;
        let visible_count = visible_count.max(3).min(box_height);

        let skip_top = (box_height - visible_count) / 2;
        let skip_bottom = box_height - visible_count - skip_top;

        let start_row = ((pty_rows.saturating_sub(box_height)) / 2).max(1);
        let start_col = ((cols as usize).saturating_sub(box_width)) / 2 + 1;

        let mut out = String::new();
        let _ = write!(out, "\x1b7");

        for (i, line) in all_lines.iter().enumerate() {
            if i < skip_top || i >= box_height - skip_bottom {
                continue;
            }
            let row = start_row + i;
            if row >= pty_rows {
                break;
            }
            let _ = write!(out, "\x1b[{};{}H{}", row, start_col, line);
        }

        let _ = write!(out, "{}\x1b8", RESET);
        out.into_bytes()
    }

    fn build_content_lines(&self, cols: u16, show_on_start: bool) -> Vec<String> {
        let cols = cols as usize;

        let qr_width = self.qr_lines.first().map(|l| l.chars().count()).unwrap_or(0);
        let url_width = self.display_url.len();
        let title = "open this terminal from any browser";
        let hints = "[c] copy    [q] quit    [esc] close";
        let toggle = "[d] show on startup _"; // placeholder for width calc
        let content_width = qr_width
            .max(url_width)
            .max(hints.len())
            .max(toggle.len())
            .max(title.len());
        let w = content_width + 6;

        if w > cols {
            return self.build_compact_lines(cols, show_on_start);
        }

        let mut lines: Vec<String> = Vec::new();

        let blank = || format!("{MODAL_BG}{}", " ".repeat(w));
        let centered_fixed = |content_styled: &str, visible_len: usize| -> String {
            let pad_left = (w.saturating_sub(visible_len)) / 2;
            let pad_right = w.saturating_sub(pad_left + visible_len);
            format!(
                "{MODAL_BG}{}{content_styled}{RESET}{MODAL_BG}{}",
                " ".repeat(pad_left),
                " ".repeat(pad_right),
            )
        };

        lines.push(blank());
        lines.push(centered_fixed(&format!("{SECONDARY}{title}"), title.len()));
        lines.push(blank());

        if !self.qr_lines.is_empty() {
            for qr_line in &self.qr_lines {
                let qr_char_width = qr_line.chars().count();
                lines.push(centered_fixed(&format!("{PRIMARY}{qr_line}"), qr_char_width));
            }
            lines.push(blank());
        }

        {
            let url = &self.url;
            let display_url = &self.display_url;
            let url_styled = format!(
                "{BOLD_PRIMARY}\x1b]8;;{url}\x07{display_url}\x1b]8;;\x07"
            );
            lines.push(centered_fixed(&url_styled, url_width));
        }

        lines.push(blank());
        let hint_styled = format!(
            "{PRIMARY}[c]{MUTED} copy    {PRIMARY}[q]{MUTED} quit    {PRIMARY}[esc]{MUTED} close"
        );
        lines.push(centered_fixed(&hint_styled, hints.len()));
        lines.push(blank());

        // Toggle line
        let (mark, mark_color) = if show_on_start { ("\u{2713}", LIVE) } else { ("\u{2717}", MUTED) };
        let toggle_styled = format!(
            "{PRIMARY}[d] {MUTED}show on startup {mark_color}{mark}"
        );
        lines.push(centered_fixed(&toggle_styled, toggle.len()));
        lines.push(blank());

        lines
    }

    fn build_compact_lines(&self, cols: usize, show_on_start: bool) -> Vec<String> {
        let w = cols.saturating_sub(4);
        if w < 10 {
            return Vec::new();
        }

        let mut lines: Vec<String> = Vec::new();

        let display = if self.display_url.len() > w {
            &self.display_url[..w]
        } else {
            &self.display_url
        };
        let pad = w.saturating_sub(display.len());
        let url = &self.url;
        lines.push(format!(
            "{MODAL_BG}{BOLD_PRIMARY}\x1b]8;;{url}\x07{display}\x1b]8;;\x07{RESET}{MODAL_BG}{}",
            " ".repeat(pad),
        ));

        let (mark, mark_color) = if show_on_start { ("\u{2713}", LIVE) } else { ("\u{2717}", MUTED) };
        let hints = format!("[c] copy [q] quit [d] startup {mark} [esc]");
        let hints_styled = format!(
            "{PRIMARY}[c]{MUTED} copy {PRIMARY}[q]{MUTED} quit {PRIMARY}[d]{MUTED} startup {mark_color}{mark} {PRIMARY}[esc]"
        );
        let hints_display = if hints.len() > w { &hints[..w] } else { &hints };
        let hpad = w.saturating_sub(hints_display.len());
        lines.push(format!(
            "{MODAL_BG}{hints_styled}{RESET}{MODAL_BG}{}",
            " ".repeat(hpad),
        ));

        lines
    }
}

fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        chars.next();
                        if nc.is_ascii_alphabetic() { break; }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        if nc == '\x07' { chars.next(); break; }
                        if nc == '\x1b' {
                            chars.next();
                            if chars.peek() == Some(&'\\') { chars.next(); }
                            break;
                        }
                        chars.next();
                    }
                }
                _ => {}
            }
        } else {
            len += 1;
        }
    }
    len
}
