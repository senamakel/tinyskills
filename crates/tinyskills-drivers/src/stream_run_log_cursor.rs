//! Opaque cursor encoding and validation for stream-backed run logs.

use serde::{Deserialize, Serialize};
use tinyskills::{RunCursor, RunLogError};

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct StreamCursorState {
    pub(super) backend: String,
    pub(super) run_id: String,
    pub(super) generation: String,
    pub(super) position: u64,
    pub(super) within: usize,
}

pub(super) fn encode_cursor(state: &StreamCursorState) -> Result<String, RunLogError> {
    let bytes =
        serde_json::to_vec(state).map_err(|error| RunLogError::Backend(error.to_string()))?;
    Ok(hex(&bytes))
}

pub(super) fn cursor_state(
    cursor: &RunCursor,
    backend: &str,
    run_id: &str,
    generation: &str,
) -> Result<(u64, usize), RunLogError> {
    validate_cursor_owner(cursor, backend, run_id)?;
    let bytes = decode_hex(cursor.as_opaque()).ok_or(RunLogError::InvalidCursor)?;
    let state: StreamCursorState =
        serde_json::from_slice(&bytes).map_err(|_| RunLogError::InvalidCursor)?;
    if state.backend != backend || state.run_id != run_id || state.generation != generation {
        return Err(RunLogError::InvalidCursor);
    }
    Ok((state.position, state.within))
}

pub(super) fn validate_cursor_owner(
    cursor: &RunCursor,
    backend: &str,
    run_id: &str,
) -> Result<(), RunLogError> {
    let bytes = decode_hex(cursor.as_opaque()).ok_or(RunLogError::InvalidCursor)?;
    let state: StreamCursorState =
        serde_json::from_slice(&bytes).map_err(|_| RunLogError::InvalidCursor)?;
    if state.backend != backend || state.run_id != run_id {
        return Err(RunLogError::InvalidCursor);
    }
    Ok(())
}

pub(super) fn hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        encoded.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
    }
    encoded
}

pub(super) fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() > 8192 || !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect()
}
