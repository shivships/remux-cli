/// Grid rendering: converts alacritty_terminal cells → escape sequences for stdout.

use std::io::Write as IoWrite;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermDamage;
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use alacritty_terminal::Term;

use super::Proxy;

pub fn write_color(buf: &mut Vec<u8>, color: Color, is_fg: bool) {
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
                _ => if is_fg { 39 } else { 49 },
            };
            let _ = write!(buf, "\x1b[{}m", code);
        }
    }
}

pub fn write_flags(buf: &mut Vec<u8>, flags: Flags) {
    if flags.contains(Flags::BOLD) { buf.extend_from_slice(b"\x1b[1m"); }
    if flags.contains(Flags::DIM) { buf.extend_from_slice(b"\x1b[2m"); }
    if flags.contains(Flags::ITALIC) { buf.extend_from_slice(b"\x1b[3m"); }
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
    if flags.contains(Flags::INVERSE) { buf.extend_from_slice(b"\x1b[7m"); }
    if flags.contains(Flags::HIDDEN) { buf.extend_from_slice(b"\x1b[8m"); }
    if flags.contains(Flags::STRIKEOUT) { buf.extend_from_slice(b"\x1b[9m"); }
}

pub fn render_line(buf: &mut Vec<u8>, term: &Term<Proxy>, line: usize, left: usize, right: usize) {
    use std::io::Write;
    let _ = write!(buf, "\x1b[{};{}H", line + 1, left + 1);

    let grid = term.grid();
    let offset = grid.display_offset() as i32;
    let grid_line = Line(line as i32 - offset);
    let row = &grid[grid_line];

    let sel_range = term.selection.as_ref().and_then(|s| s.to_range(term));

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

        let vis_flags = cell.flags & !(Flags::WRAPLINE | Flags::WIDE_CHAR);
        if cell.fg != cur_fg || cell.bg != cur_bg || vis_flags != cur_flags || selected != cur_selected {
            buf.extend_from_slice(b"\x1b[0m");
            if selected {
                buf.extend_from_slice(b"\x1b[7m");
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

pub fn render_full(buf: &mut Vec<u8>, term: &Term<Proxy>) {
    buf.extend_from_slice(b"\x1b[H\x1b[2J");
    for line in 0..term.screen_lines() {
        render_line(buf, term, line, 0, term.columns().saturating_sub(1));
    }
}

pub fn render_damage(buf: &mut Vec<u8>, term: &mut Term<Proxy>) {
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

pub fn position_cursor(buf: &mut Vec<u8>, term: &Term<Proxy>) {
    use std::io::Write;
    let cursor = term.grid().cursor.point;
    let _ = write!(buf, "\x1b[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1);
}

pub fn draw_bar(stdout: &mut impl IoWrite, cols: u16, rows: u16, bar_url: Option<&str>, slug: Option<&str>, clients: usize) {
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

pub fn base64_encode(data: &[u8]) -> String {
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
