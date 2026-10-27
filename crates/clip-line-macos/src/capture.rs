//! NSPasteboard changeCount poll every 300 ms (SPEC capture_macos).
//! No OS notification API exists for macOS pasteboards; polling is the norm.
//!
//! `NSPasteboard` is `!Send` in objc2 (AppKit objects are main-threadish), so
//! `MacCapture` stores no pasteboard handle: every poll re-fetches
//! `generalPasteboard()` — safe from background threads for this read-only
//! use, and keeps the struct `Send` as `Capture` requires.

use clip_line_core::capture::{Block, Capture, ClipEvent, Format};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL, NSWorkspace};
use std::time::Duration;

const POLL: Duration = Duration::from_millis(300);

pub struct MacCapture {
    last_count: isize,
    suppress_count: Option<isize>,
}

/// True when an event with changeCount `ev` must be ignored because it matches
/// a self-originated write (`seen`). Same shape as the win adapter.
pub fn should_ignore(seen: Option<isize>, ev: isize) -> bool {
    seen == Some(ev)
}

impl MacCapture {
    pub fn new() -> Self {
        unsafe {
            let count = NSPasteboard::generalPasteboard().changeCount();
            Self {
                last_count: count,
                suppress_count: None,
            }
        }
    }

    /// Tell the capture to ignore the event at changeCount `n` (the write
    /// `MacPaster` just made; see `MacPaster::set_clipboard`).
    pub fn suppress_next(&mut self, n: isize) {
        self.suppress_count = Some(n);
    }
}

impl Capture for MacCapture {
    fn next_event(&mut self) -> anyhow::Result<ClipEvent> {
        loop {
            std::thread::sleep(POLL);
            unsafe {
                let pb = NSPasteboard::generalPasteboard();
                let count = pb.changeCount();
                if count == self.last_count {
                    continue;
                }
                self.last_count = count;
                if should_ignore(self.suppress_count, count) {
                    self.suppress_count = None; // one-shot: next write is foreign again
                    continue;
                }
                let ev = read_pasteboard(&pb)?;
                if !ev.blocks.is_empty() {
                    return Ok(ev);
                }
            }
        }
    }
}

/// Pure mapping: (UTI string, payload bytes) -> Block. Unit-test surface.
/// Unknown UTI -> Registered(0, uti): the numeric id slot is Windows-CF-only;
/// macOS rows keep the name so the envelope round-trip preserves it.
pub fn map_block(uti: &str, bytes: Vec<u8>) -> Block {
    let format = match uti {
        "public.utf8-plain-text" | "NSStringPboardType" => Format::UnicodeText,
        // Images ride the DIB id (8) so the store sees an image row; the name
        // keeps the real encoding for the paster to restore.
        // envelope id must NOT be 8 (CF_DIB byte layout): keep a distinct
        // registered id so the store never mistakes PNG/TIFF bytes for a DIB.
        "public.tiff" => Format::Registered(2, "TIFF".into()),
        "public.png" => Format::Registered(2, "PNG".into()),
        "public.file-url" => Format::FileDrop,
        other => Format::Registered(0, other.to_string()),
    };
    Block { format, bytes }
}

/// Map a pasteboard type string (the NSPasteboardType constants are UTI
/// strings: String == "public.utf8-plain-text", PNG == "public.png", ...) to
/// the UTI used by `map_block`. None when `t` is not one we read directly.
pub fn map_ns_type(t: &str) -> Option<&'static str> {
    match t {
        "public.utf8-plain-text" | "NSStringPboardType" => Some("public.utf8-plain-text"),
        "public.png" => Some("public.png"),
        "public.tiff" => Some("public.tiff"),
        "public.file-url" => Some("public.file-url"),
        _ => None,
    }
}

/// Decode UTF-16LE storage bytes (wide-NUL terminated or not) to a String.
pub fn utf16_bytes_to_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .take_while(|c| !(c[0] == 0 && c[1] == 0))
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// String -> UTF-16LE bytes, no terminator (inverse of utf16_bytes_to_string).
pub fn string_to_utf16_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// Split a UTF-16LE NUL-separated file-URL list into Strings (storage form
/// written by `read_file_urls`; consumed by the paster).
pub fn file_urls_from_bytes(bytes: &[u8]) -> Vec<String> {
    utf16_bytes_to_string(bytes)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

unsafe fn read_pasteboard(pb: &NSPasteboard) -> anyhow::Result<ClipEvent> {
    unsafe {
        let (app, title) = frontmost_app();
        let mut blocks = Vec::new();
        let Some(types) = pb.types() else {
            return Ok(ClipEvent {
                blocks,
                source_app: app,
                source_title: title,
            });
        };
        for t in types.iter() {
            // Type strings are UTF-16; lossy is fine for dispatching.
            let t_str = t.to_string();
            match map_ns_type(&t_str) {
                Some("public.file-url") => {
                    // One block for the whole file list, `file://` URLs joined.
                    if let Some(data) = read_file_urls(pb) {
                        blocks.push(Block {
                            format: Format::FileDrop,
                            bytes: data,
                        });
                    }
                }
                Some(uti) => {
                    if let Some(d) = pb.dataForType(&*t) {
                        let bytes = d.to_vec();
                        // Text: store as UTF-16LE like every other adapter;
                        // dataForType(String) yields UTF-8 bytes on macOS.
                        let bytes = if uti == "public.utf8-plain-text" {
                            string_to_utf16_bytes(&String::from_utf8_lossy(&bytes))
                        } else {
                            bytes
                        };
                        blocks.push(map_block(uti, bytes));
                    }
                }
                None => {
                    // Non-core type: keep as registered block when data exists.
                    if let Some(d) = pb.dataForType(&*t) {
                        blocks.push(map_block(&t_str, d.to_vec()));
                    }
                }
            }
        }
        Ok(ClipEvent {
            blocks,
            source_app: app,
            source_title: title,
        })
    }
}

/// All public.file-url items as one UTF-16LE URL list, NUL-separated with a
/// single trailing NUL (FileDrop storage convention; the Windows paster builds
/// its DROPFILES structure from a list like this).
unsafe fn read_file_urls(pb: &NSPasteboard) -> Option<Vec<u8>> {
    unsafe {
        let items = pb.pasteboardItems()?;
        let mut urls: Vec<String> = Vec::new();
        for item in items.iter() {
            urls.push(item.stringForType(NSPasteboardTypeFileURL)?.to_string());
        }
        if urls.is_empty() {
            return None;
        }
        Some(file_urls_to_bytes(&urls))
    }
}

/// Join file URLs into the UTF-16LE NUL-separated storage form. Pure; shared
/// with the paster's round-trip.
pub fn file_urls_to_bytes(urls: &[String]) -> Vec<u8> {
    let joined = urls.join("\0");
    let mut units: Vec<u16> = joined.encode_utf16().collect();
    units.push(0); // single trailing NUL terminates the last entry
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// Frontmost application name and title. `NSRunningApplication` exposes no
/// window title accessor; title stays "" until a CGWindow-list lookup lands.
/// Unknown steps degrade to "" — never block capture on lookup failure.
unsafe fn frontmost_app() -> (String, String) {
    unsafe {
        let ws = NSWorkspace::sharedWorkspace();
        let app = ws
            .frontmostApplication()
            .and_then(|a| a.localizedName().map(|n| n.to_string()))
            .unwrap_or_default();
        (app, String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        file_urls_from_bytes, file_urls_to_bytes, map_block, map_ns_type, should_ignore,
        string_to_utf16_bytes, utf16_bytes_to_string,
    };
    use clip_line_core::capture::Format;

    #[test]
    fn suppression_matches_only_the_recorded_count() {
        assert!(should_ignore(Some(42), 42), "self-originated event ignored");
        assert!(!should_ignore(Some(42), 43), "next foreign event passes");
        assert!(
            !should_ignore(None, 42),
            "no suppression armed: everything passes"
        );
    }

    #[test]
    fn core_utis_map_to_stable_formats() {
        assert_eq!(
            map_block("public.utf8-plain-text", vec![]).format,
            Format::UnicodeText
        );
        assert_eq!(
            map_block("public.file-url", vec![]).format,
            Format::FileDrop
        );
        match map_block("public.png", vec![]).format {
            Format::Registered(id, name) => {
                assert_eq!(id, 8, "PNG rides the DIB id so the store sees an image");
                assert_eq!(name, "PNG");
            }
            f => panic!("png mapped to {f:?}"),
        }
        match map_block("com.apple.custom-uti", vec![1, 2]).format {
            Format::Registered(0, name) => assert_eq!(name, "com.apple.custom-uti"),
            f => panic!("unknown uti mapped to {f:?}"),
        }
    }

    #[test]
    fn ns_type_strings_map_to_utis() {
        for (raw, uti) in [
            ("public.utf8-plain-text", "public.utf8-plain-text"),
            ("public.png", "public.png"),
            ("public.tiff", "public.tiff"),
            ("public.file-url", "public.file-url"),
            ("NSStringPboardType", "public.utf8-plain-text"),
        ] {
            assert_eq!(map_ns_type(raw), Some(uti), "{raw}");
        }
        assert_eq!(map_ns_type("dyn.aah"), None, "unknown type");
    }

    #[test]
    fn utf16_round_trip_strips_and_restores_payload() {
        let s = "clipline 你好 🦀";
        let mut bytes = string_to_utf16_bytes(s);
        bytes.extend_from_slice(&[0, 0]); // storage form: terminated
        assert_eq!(utf16_bytes_to_string(&bytes), s);
        assert_eq!(utf16_bytes_to_string(&string_to_utf16_bytes("")), "");
    }

    #[test]
    fn file_url_list_round_trips() {
        let urls: Vec<String> = ["file:///Users/asd/a.txt", "file:///Users/asd/b%20c.png"]
            .map(String::from)
            .into();
        let bytes = file_urls_to_bytes(&urls);
        assert_eq!(file_urls_from_bytes(&bytes), urls);
        assert!(
            file_urls_from_bytes(&[0, 0]).is_empty(),
            "empty list stays empty"
        );
    }
}
