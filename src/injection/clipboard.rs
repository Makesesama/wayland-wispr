//! Native data-control clipboard via `wl-clipboard-rs` (a protocol library,
//! not the `wl-copy` executable).

use std::io::Read;

use wl_clipboard_rs::{copy, paste};

const READBACK_SLACK: usize = 16;

/// Offer `text` as the regular clipboard selection on all seats.
///
/// `wl-clipboard-rs` keeps serving paste requests from a background thread
/// until another client takes the selection. After setting it, the
/// selection is read back so a following paste chord cannot race the
/// compositor processing our `set_selection`.
pub fn set_text(text: &str) -> Result<(), String> {
    copy::Options::new()
        .copy(
            copy::Source::Bytes(text.as_bytes().into()),
            copy::MimeType::Text,
        )
        .map_err(|e| format!("clipboard copy failed: {}", error_chain(&e)))?;

    let (pipe, _mime) = paste::get_contents(
        paste::ClipboardType::Regular,
        paste::Seat::Unspecified,
        paste::MimeType::Text,
    )
    .map_err(|e| format!("clipboard verification failed: {}", error_chain(&e)))?;

    let mut readback = Vec::with_capacity(text.len());
    pipe.take((text.len() + READBACK_SLACK) as u64)
        .read_to_end(&mut readback)
        .map_err(|e| format!("clipboard verification failed: {e}"))?;

    if readback != text.as_bytes() {
        return Err("clipboard verification failed: selection does not match the result".into());
    }
    Ok(())
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut source = e.source();
    while let Some(inner) = source {
        s.push_str(": ");
        s.push_str(&inner.to_string());
        source = inner.source();
    }
    s
}
