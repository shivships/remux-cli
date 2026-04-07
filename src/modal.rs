use std::fmt::Write as FmtWrite;

const GREEN: &str = "\x1b[38;2;180;189;104m";
const BOLD_GREEN: &str = "\x1b[1;38;2;180;189;104m";
const DIM_GRAY: &str = "\x1b[38;2;128;128;128m";
const BRIGHT_WHITE: &str = "\x1b[97m";
const MODAL_BG: &str = "\x1b[48;2;45;47;51m";
const RESET: &str = "\x1b[0m";
const COPIED_GREEN: &str = "\x1b[1;38;2;180;189;104m";

pub struct ModalContent {
    pub url: String,
    pub display_url: String,
    qr_lines: Vec<String>,
}

impl ModalContent {
    pub fn new(slug: &str) -> Self {
        let url = format!("https://remux.sh/{}", slug);
        let display_url = format!("remux.sh/{}", slug);

        let qr_lines = if let Ok(qr) = fast_qr::QRBuilder::new(url.as_str()).build() {
            qr.to_str().lines().map(|l| l.to_string()).collect()
        } else {
            Vec::new()
        };

        Self { url, display_url, qr_lines }
    }

    pub fn render_frame(&self, cols: u16, rows: u16, frame: u8) -> Vec<u8> {
        let pty_rows = rows.saturating_sub(1).max(1) as usize;
        let all_lines = self.build_content_lines(cols);
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

    pub fn render_full(&self, cols: u16, rows: u16) -> Vec<u8> {
        self.render_frame(cols, rows, 2)
    }

    pub fn render_copied_flash(&self, cols: u16, rows: u16) -> Vec<u8> {
        let pty_rows = rows.saturating_sub(1).max(1) as usize;
        let all_lines = self.build_content_lines(cols);
        let box_height = all_lines.len();
        let box_width = all_lines.iter().map(|l| visible_len(l)).max().unwrap_or(0);

        if box_height < 2 {
            return Vec::new();
        }

        let keybind_idx = box_height - 2;
        let start_row = ((pty_rows.saturating_sub(box_height)) / 2).max(1);
        let start_col = ((cols as usize).saturating_sub(box_width)) / 2 + 1;

        let row = start_row + keybind_idx;
        if row >= pty_rows {
            return Vec::new();
        }

        let copied_text = "Copied!";
        let pad_left = (box_width.saturating_sub(copied_text.len())) / 2;
        let pad_right = box_width.saturating_sub(pad_left + copied_text.len());

        let mut out = String::new();
        let _ = write!(
            out,
            "\x1b7\x1b[{};{}H{}{}{}{}{}{}{}\x1b8",
            row, start_col,
            MODAL_BG, " ".repeat(pad_left),
            COPIED_GREEN, copied_text, RESET,
            MODAL_BG, " ".repeat(pad_right),
        );
        out.into_bytes()
    }

    fn build_content_lines(&self, cols: u16) -> Vec<String> {
        let cols = cols as usize;

        let qr_width = self.qr_lines.first().map(|l| l.chars().count()).unwrap_or(0);
        let url_width = self.display_url.len();
        let title = "open this terminal from any browser";
        let hints = "[c] copy    [q] quit    [esc] close";
        let content_width = qr_width.max(url_width).max(hints.len()).max(title.len());
        let w = content_width + 6;

        if w > cols {
            return self.build_compact_lines(cols);
        }

        let mut lines: Vec<String> = Vec::new();

        let blank = || format!("{}{}{:w$}", MODAL_BG, GREEN, "");
        let centered = |text: &str, color: &str, len: usize| -> String {
            let pad_left = (w.saturating_sub(len)) / 2;
            let pad_right = w.saturating_sub(pad_left + len);
            format!(
                "{}{}{}{}{}{}",
                MODAL_BG, " ".repeat(pad_left), color, text, RESET, MODAL_BG,
            ) + &" ".repeat(pad_right)
        };

        lines.push(blank());
        lines.push(centered(title, DIM_GRAY, title.len()));
        lines.push(blank());

        if !self.qr_lines.is_empty() {
            for qr_line in &self.qr_lines {
                let qr_char_width = qr_line.chars().count();
                let pad_left = (w.saturating_sub(qr_char_width)) / 2;
                let pad_right = w.saturating_sub(pad_left + qr_char_width);
                lines.push(format!(
                    "{}{}{}{}{}",
                    MODAL_BG, " ".repeat(pad_left), BRIGHT_WHITE, qr_line, RESET,
                ) + &format!("{}{}", MODAL_BG, " ".repeat(pad_right)));
            }
            lines.push(blank());
        }

        {
            let pad_left = (w.saturating_sub(url_width)) / 2;
            let pad_right = w.saturating_sub(pad_left + url_width);
            lines.push(format!(
                "{}{}{}\x1b]8;;{}\x07{}\x1b]8;;\x07{}{}",
                MODAL_BG, " ".repeat(pad_left), BOLD_GREEN,
                self.url, self.display_url, RESET, MODAL_BG,
            ) + &" ".repeat(pad_right));
        }

        lines.push(blank());
        lines.push(centered(hints, DIM_GRAY, hints.len()));
        lines.push(blank());

        lines
    }

    fn build_compact_lines(&self, cols: usize) -> Vec<String> {
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
        lines.push(format!(
            "{}{}\x1b]8;;{}\x07{}\x1b]8;;\x07{}{}{}",
            MODAL_BG, BOLD_GREEN, self.url, display, RESET, MODAL_BG, " ".repeat(pad),
        ));

        let hints = "[c] copy [q] quit [esc] close";
        let hints_display = if hints.len() > w { &hints[..w] } else { hints };
        let hpad = w.saturating_sub(hints_display.len());
        lines.push(format!(
            "{}{}{}{}{}",
            MODAL_BG, DIM_GRAY, hints_display, RESET,
            format!("{}{}", MODAL_BG, " ".repeat(hpad)),
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
