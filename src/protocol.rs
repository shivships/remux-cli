use serde::Deserialize;

pub const MSG_TERMINAL_DATA: u8 = 0x00;
pub const MSG_TERMINAL_RESIZE: u8 = 0x01;

#[derive(Debug, Deserialize)]
pub struct ResizePayload {
    pub cols: u16,
    pub rows: u16,
}
