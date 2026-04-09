/// Stdin event types and SGR mouse parser.

pub enum StdinEvent {
    Data(Vec<u8>),
    ScrollUp(i32),
    ScrollDown(i32),
    Mouse(Vec<u8>),
    Focus(Vec<u8>),
    SelectStart { col: u16, row: u16, alt: bool },
    SelectUpdate(u16, u16),
    SelectEnd,
    ModalToggle,
    ModalCopy,
    ModalQuit,
    ModalDismiss,
}

/// Parse stdin bytes, extracting scroll/mouse/modal events from SGR mouse sequences.
/// SGR mouse format: \x1b[<button;col;row[Mm]
/// Button 64 = scroll up, 65 = scroll down.
pub fn parse_stdin(bytes: &[u8], inner_wants_mouse: bool, modal_open: bool) -> Vec<StdinEvent> {
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
                _ => { i += 1; continue; }
            }
        }

        // Check for escape sequences
        if bytes[i] == 0x1b {
            let start = i;

            // SGR mouse: \x1b[<button;col;row[Mm]
            if i + 2 < bytes.len() && bytes[i + 1] == b'[' && bytes[i + 2] == b'<' {
                i += 3;
                let seq_start = i;
                while i < bytes.len() && bytes[i] != b'M' && bytes[i] != b'm' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                    let params = &bytes[seq_start..i - 1];
                    if let Ok(params_str) = std::str::from_utf8(params) {
                        let parts: Vec<&str> = params_str.split(';').collect();
                        if parts.len() >= 3 {
                            let button = parts[0].parse::<u32>().unwrap_or(999);
                            let col = parts[1].parse::<u16>().unwrap_or(1).saturating_sub(1);
                            let row = parts[2].parse::<u16>().unwrap_or(1).saturating_sub(1);
                            let is_release = bytes[i - 1] == b'm';

                            if inner_wants_mouse {
                                events.push(StdinEvent::Mouse(bytes[start..i].to_vec()));
                                continue;
                            }

                            let has_alt = button & 8 != 0;
                            let base_button = button & !0b11100;

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
                    continue;
                } else {
                    break;
                }
            }

            // Focus in/out: \x1b[I / \x1b[O — forward without triggering scroll-to-bottom
            if i + 2 < bytes.len()
                && bytes[i + 1] == b'['
                && (bytes[i + 2] == b'I' || bytes[i + 2] == b'O')
            {
                events.push(StdinEvent::Focus(bytes[i..i + 3].to_vec()));
                i += 3;
                continue;
            }

            // Other escape sequences — forward as data
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'~') {
                    i += 1;
                }
                if i < bytes.len() { i += 1; }
            } else if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                i += 2;
                while i < bytes.len() && bytes[i] != 0x07 {
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2; break;
                    }
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == 0x07 { i += 1; }
            } else {
                i += 1;
                if i < bytes.len() { i += 1; }
            }
            events.push(StdinEvent::Data(bytes[start..i].to_vec()));
            continue;
        }

        // Regular bytes
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
