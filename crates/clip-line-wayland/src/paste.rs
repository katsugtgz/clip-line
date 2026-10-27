//! Clipboard set via data-control copy; injection via ydotool/wtype (SPEC Q7:
//! Wayland paste-back is manual v1 — no synthetic Ctrl+V on Wayland itself).

use clip_line_core::capture::{Block, Format};
use clip_line_core::paste::{ClipData, PasteMode, Paster};
use std::process::Command;
use wl_clipboard_rs::copy::{self, MimeSource, MimeType as CopyMime, ServeRequests, Source};

/// Injection helpers tried in order. Both talk uinput/protocols outside
/// Wayland's security model, so each needs manual setup once.
pub const INJECT_TOOLS: &[&str] = &["ydotool", "wtype"];

/// External command present AND executable on PATH? Pure — unit-testable
/// without Linux. A non-executable file would only fail at spawn time with
/// "Permission denied", after the other helper was skipped.
pub fn tool_available(tool: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':').any(|dir| {
        let p = std::path::Path::new(dir).join(tool);
        is_executable_file(&p)
    })
}

#[cfg(unix)]
fn is_executable_file(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(_p: &std::path::Path) -> bool {
    false // Windows builds never inject through ydotool/wtype
}

/// First available injection tool; None → setup-hint error text.
pub fn pick_tool() -> Option<&'static str> {
    INJECT_TOOLS.iter().copied().find(|t| tool_available(t))
}

/// Message for the missing-tool case (setup hint per SPEC Q7).
pub fn missing_tool_error(mode: PasteMode) -> String {
    let how = match mode {
        PasteMode::InjectCtrlV => {
            "auto-paste needs one of:\n  ydotool: install, enable ydotoold service, set $YDOTOOL_SOCKET\n  wtype: install (per-user, no daemon)"
        }
        _ => {
            "typing emulation needs one of:\n  ydotool: install, enable ydotoold service, set $YDOTOOL_SOCKET\n  wtype: install (per-user, no daemon)"
        }
    };
    format!(
        "Wayland key injection is manual-setup v1: none of [{}] found.\n{how}",
        INJECT_TOOLS.join(", ")
    )
}

pub struct WaylandPaster;

impl Paster for WaylandPaster {
    fn paste(&self, data: &ClipData, mode: PasteMode) -> anyhow::Result<()> {
        set_clipboard(&data.blocks)?;
        match mode {
            PasteMode::SetOnly => Ok(()),
            PasteMode::InjectCtrlV => {
                let tool = pick_tool().ok_or_else(|| anyhow::anyhow!(missing_tool_error(mode)))?;
                let status = Command::new(tool).arg("key").arg("ctrl+v").status()?;
                if status.success() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("{tool} key ctrl+v failed: {status}"))
                }
            }
            PasteMode::TypeIn => {
                let tool = pick_tool().ok_or_else(|| anyhow::anyhow!(missing_tool_error(mode)))?;
                let text = text_of(&data.blocks)?;
                let status = match tool {
                    "ydotool" => Command::new("ydotool").arg("type").arg(&text).status()?,
                    _ => Command::new("wtype").arg(&text).status()?,
                };
                if status.success() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("{tool} type failed: {status}"))
                }
            }
        }
    }
}

/// Data-control copy: clipboard-set only (no window focus needed — that is
/// the point of data-control for terminal apps).
pub fn set_clipboard(blocks: &[Block]) -> anyhow::Result<()> {
    let sources = blocks
        .iter()
        .filter_map(block_source)
        .collect::<Vec<MimeSource>>();
    if sources.is_empty() {
        anyhow::bail!("no blocks with a Wayland-representable format");
    }
    let mut opts = copy::Options::new();
    opts.serve_requests(ServeRequests::Unlimited);
    opts.copy_multi(sources)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// One block → one (bytes, mime) source. Unknown formats dropped.
fn block_source(b: &Block) -> Option<MimeSource> {
    let (bytes, mime): (Vec<u8>, CopyMime) = match &b.format {
        Format::UnicodeText => (utf16_to_utf8(&b.bytes), CopyMime::Text),
        Format::Registered(_, name) if name == "image/png" => {
            (b.bytes.clone(), CopyMime::Specific("image/png".into()))
        }
        Format::FileDrop => (
            file_drop_to_uri_list(&b.bytes),
            CopyMime::Specific("text/uri-list".into()),
        ),
        // ponytail: DIB/DIBV5 have no Wayland mime; convert via image crate when image clips matter here
        _ => return None,
    };
    Some(MimeSource {
        source: Source::Bytes(bytes.into_boxed_slice()),
        mime_type: mime,
    })
}

/// Stored UTF-16LE (+NULs) → Wayland UTF-8.
pub fn utf16_to_utf8(bytes: &[u8]) -> Vec<u8> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units).into_bytes()
}

/// Stored FileDrop form (UTF-16LE, NUL-separated) -> full UTF-8
/// text/uri-list: every entry, newline-separated, CRLF line ends per RFC 2483.
pub fn file_drop_to_uri_list(bytes: &[u8]) -> Vec<u8> {
    let urls: Vec<String> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect::<Vec<u16>>()
        .split(|&u| u == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf16_lossy(s))
        .collect();
    let mut out = String::new();
    for u in urls {
        out.push_str(&u);
        out.push_str(
            "
",
        );
    }
    out.into_bytes()
}

fn text_of(blocks: &[Block]) -> anyhow::Result<String> {
    blocks
        .iter()
        .find(|b| b.format == Format::UnicodeText)
        .map(|b| String::from_utf8_lossy(&utf16_to_utf8(&b.bytes)).into_owned())
        .ok_or_else(|| anyhow::anyhow!("no text block in this clip"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tool_message_names_both() {
        let msg = missing_tool_error(PasteMode::InjectCtrlV);
        assert!(msg.contains("ydotool") && msg.contains("wtype"), "{msg}");
        assert!(msg.contains("YDOTOOL_SOCKET"), "{msg}");
    }

    #[test]
    fn utf16_round_trip() {
        let units: Vec<u16> = "hello wörld 你好"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(
            utf16_to_utf8(&bytes),
            b"hello w\xc3\xb6rld \xe4\xbd\xa0\xe5\xa5\xbd"
        );
    }

    #[test]
    fn utf16_stops_at_first_nul() {
        // trailing allocation noise after terminator must not leak
        let mut bytes = to_bytes(&["a", "\0"]);
        bytes.extend_from_slice(b"garbage");
        assert_eq!(utf16_to_utf8(&bytes), b"a");
    }

    fn to_bytes(strs: &[&str]) -> Vec<u8> {
        strs.iter()
            .flat_map(|s| s.encode_utf16())
            .flat_map(|u| u.to_le_bytes())
            .collect()
    }

    #[test]
    fn tool_check_exists_only() {
        // sh exists everywhere unix; a nonsense name must not
        assert!(!tool_available("definitely-not-a-tool-xyz"));
    }
}
