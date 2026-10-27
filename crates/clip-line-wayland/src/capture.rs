//! wlr-data-control / ext-data-control watch (GNOME 48+ native, wlroots, KDE).

use clip_line_core::capture::{Block, Capture, ClipEvent, Format};
use std::io::Read;
use wl_clipboard_rs::paste::Error as WlError;
use wl_clipboard_rs::watch::{ClipboardEvent, ClipboardType, Offer, Watcher};

/// Best MIME per offer. Ranked: utf8 text, png image, uri-list, plain text.
/// Everything else (aliases, html, obscure types) dropped — v1 archives one
/// block per clip, matching how the headless demo feeds the store.
pub fn block_mime(mimes: &[String]) -> Option<&String> {
    let rank = |m: &str| match m {
        "text/plain;charset=utf-8" => 0,
        "image/png" => 1,
        "text/uri-list" => 2,
        "text/plain" => 3,
        _ => usize::MAX,
    };
    mimes
        .iter()
        .min_by_key(|m| rank(m))
        .filter(|m| rank(m) < usize::MAX)
}

/// True when the error means the compositor lacks clipboard control support.
/// wl-clipboard-rs reports absent protocols via `MissingProtocol`
/// (wlr-data-control absent in GNOME ≤47; ext-data-control native from 48).
fn is_unsupported(err: &WlError) -> Option<(&'static str, u32)> {
    match err {
        WlError::MissingProtocol { name, version } => Some((name, *version)),
        WlError::PrimarySelectionUnsupported => Some(("primary-selection", 0)),
        _ => None,
    }
}

/// Human message for a compositor-support error, including the desktop
/// detected via XDG_CURRENT_DESKTOP (issue story 7: capture off must say why).
pub fn unsupported_message(err: &WlError) -> Option<String> {
    let Some((name, _version)) = is_unsupported(err) else {
        return None;
    };
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unknown".into());
    let gnome = desktop.to_ascii_lowercase().contains("gnome");
    let why = if gnome {
        "GNOME ships the data-control protocol only from 48 (ext-data-control); \
         this GNOME session is older and has no wlr-data-control"
    } else {
        "this compositor ships neither wlr-data-control nor ext-data-control"
    };
    Some(format!(
        "Wayland capture unavailable: missing protocol {name} on desktop {desktop} (XDG_CURRENT_DESKTOP={desktop}) — {why}. Use a wlroots/KDE session or GNOME 48+, or run \
         under XWayland/X11."
    ))
}

/// Any wl-clipboard error → anyhow; compositor gaps get the long message.
pub fn describe(err: WlError) -> anyhow::Error {
    match unsupported_message(&err) {
        Some(msg) => anyhow::anyhow!(msg),
        None => anyhow::anyhow!("{err}"),
    }
}

pub struct WaylandCapture {
    watcher: Watcher,
}

impl WaylandCapture {
    pub fn new() -> anyhow::Result<Self> {
        Ok(WaylandCapture {
            watcher: Watcher::new(
                ClipboardType::Regular,
                wl_clipboard_rs::paste::Seat::Unspecified,
            )
            .map_err(describe)?,
        })
    }
}

impl Capture for WaylandCapture {
    fn next_event(&mut self) -> anyhow::Result<ClipEvent> {
        loop {
            let Some(ev) = self.watcher.next_event().map_err(describe)? else {
                anyhow::bail!("watcher cancelled");
            };
            match ev {
                ClipboardEvent::Cleared { .. } => continue, // empty clipboard: no clip
                ClipboardEvent::Changed {
                    mime_types,
                    mut offer,
                    ..
                } => {
                    if let Some(ev) = read_offer(&mime_types, &mut offer)? {
                        return Ok(ev);
                    }
                    // zero recognized formats: keep waiting (matches WinCapture)
                }
            }
        }
    }
}

fn read_offer(mimes: &[String], offer: &mut Offer<'_>) -> anyhow::Result<Option<ClipEvent>> {
    let Some(mime) = block_mime(mimes) else {
        return Ok(None);
    };
    let mut pipe = offer.receive(mime).map_err(describe)?;
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    let format = match mime.as_str() {
        // id must not collide with 8 (CF_DIB) or 15 (CF_HDROP): PNG bytes
        // are not DIBs; keep a distinct registered id with the mime name.
        "image/png" => Format::Registered(2, "image/png".into()),
        "text/uri-list" => Format::FileDrop,
        _ => Format::UnicodeText,
    };
    match format {
        Format::UnicodeText => {
            // Wayland text offers are UTF-8 without a terminator; storage keeps
            // the Windows shape (UTF-16LE + wide NUL) so the envelope matches.
            // Trailing CRLF is payload, not framing — it is not trimmed.
            bytes = to_win_text(&String::from_utf8_lossy(&bytes));
        }
        Format::FileDrop => {
            // Storage form: UTF-16LE, NUL-separated, one trailing wide NUL
            // (same as the macOS adapter); the paster re-emits a URI list.
            bytes = uri_list_to_storage(&String::from_utf8_lossy(&bytes));
        }
        _ => {}
    }
    Ok(Some(ClipEvent {
        blocks: vec![Block { format, bytes }],
        // data-control exposes no window context; source app stays unknown
        // (story 10 Windows-only) until a portal route exists.
        source_app: String::new(),
        source_title: String::new(),
    }))
}

/// UTF-8 → UTF-16LE + wide NUL terminator (storage/envelope format).
pub fn to_win_text(s: &str) -> Vec<u8> {
    let mut out: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    out.extend_from_slice(&[0, 0]);
    out
}

/// text/uri-list (CRLF lines, `#` comments) -> storage form: UTF-16LE,
/// NUL-separated, one trailing wide NUL. Empty lines and comments dropped.
pub fn uri_list_to_storage(list: &str) -> Vec<u8> {
    let urls: Vec<String> = list
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect();
    let joined = urls.join("\0");
    let mut units: Vec<u16> = joined.encode_utf16().collect();
    units.push(0);
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mimes(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn picks_best_mime() {
        assert_eq!(
            block_mime(&mimes(&["text/plain;charset=utf-8"])).map(String::as_str),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(
            block_mime(&mimes(&["image/png", "text/plain"])).map(String::as_str),
            Some("image/png")
        );
    }

    #[test]
    fn skips_unknown_mimes() {
        assert!(block_mime(&mimes(&["application/octet-stream", "text/html"])).is_none());
    }

    #[test]
    fn missing_protocol_builds_message() {
        let e = WlError::MissingProtocol {
            name: "zwlr_data_control_device_manager_v1",
            version: 2,
        };
        let msg = unsupported_message(&e).unwrap();
        assert!(msg.contains("data-control"), "{msg}");
        assert!(msg.contains("XDG"), "{msg}");
    }

    #[test]
    fn transport_errors_untouched() {
        assert!(unsupported_message(&WlError::NoSeats).is_none());
    }

    #[test]
    fn utf8_to_win_text_round_shape() {
        let b = to_win_text("hi");
        assert_eq!(b, vec![b'h', 0, b'i', 0, 0, 0]);
    }

    #[test]
    fn uri_list_to_storage_round_trip() {
        let storage = uri_list_to_storage("file:///a\r\nfile:///b\r\n# comment\r\n");
        // two entries NUL-separated + trailing wide NUL
        let want: Vec<u8> = "file:///a\0file:///b\0"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(storage, want);
    }

    #[test]
    fn text_payload_keeps_trailing_newline() {
        // cubic 6187: trailing CRLF is payload; only the wide NUL terminator
        // is added.
        let b = to_win_text("line\r\n");
        assert_eq!(b.len(), 8, "CRLF kept + wide NUL");
    }
}
