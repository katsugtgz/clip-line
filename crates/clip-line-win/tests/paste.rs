#![cfg(windows)]

//! Round-trip: WinPaster::set_clipboard writes CF_UNICODETEXT, we read it back
//! with our own raw-clipboard code (no extra deps) and assert payload equality.

use clip_line_core::capture::{Block, Format};
use clip_line_core::paste::ClipData;
use clip_line_win::WinPaster;
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

fn read_clipboard_unicode() -> Option<Vec<u16>> {
    unsafe {
        if OpenClipboard(None).is_err() {
            return None;
        }
        let r = (|| {
            let handle: HANDLE = GetClipboardData(13).ok()?;
            let h = HGLOBAL(handle.0);
            let p = GlobalLock(h);
            if p.is_null() {
                return None;
            }
            let alloc = GlobalSize(h);
            let units = std::slice::from_raw_parts(p as *const u16, alloc / 2);
            let n = units.iter().position(|&u| u == 0).unwrap_or(units.len());
            let out = units[..n].to_vec();
            let _ = GlobalUnlock(h);
            Some(out)
        })();
        let _ = CloseClipboard();
        r
    }
}

#[test]
fn set_only_text_round_trip() {
    let text = "clipline round-trip 你好 🦀";
    let units: Vec<u16> = text.encode_utf16().collect();
    let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    let data = ClipData {
        blocks: vec![Block {
            format: Format::UnicodeText,
            bytes,
        }],
    };
    // Other processes can hold the clipboard; retry the set side.
    let mut seq = 0;
    for _ in 0..20 {
        if let Ok(s) = WinPaster.set_clipboard(&data) {
            seq = s;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(seq != 0, "could not open/set clipboard after retries");
    let mut read = None;
    for _ in 0..20 {
        if let Some(u) = read_clipboard_unicode() {
            read = Some(u);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        read.as_deref(),
        Some(units.as_slice()),
        "clipboard text mismatch"
    );
}
