//! SetClipboardData + SendInput (Ctrl+V injection / Unicode typing).

use clip_line_core::capture::{Block, Format};
use clip_line_core::paste::{ClipData, PasteMode, Paster};
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::Globalization::{CP_ACP, MultiByteToWideChar};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
    VIRTUAL_KEY,
};
use windows::core::PCWSTR;

/// GlobalFree is not exposed by the windows 0.62 crate; declare it directly.
#[allow(non_snake_case)]
unsafe fn GlobalFree(h: HGLOBAL) -> HGLOBAL {
    unsafe {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GlobalFree(hmem: HGLOBAL) -> HGLOBAL;
        }
        GlobalFree(h)
    }
}

use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_V};

pub struct WinPaster;

impl WinPaster {
    /// Puts `data` on the clipboard (SetOnly semantics) and returns the new
    /// clipboard sequence number, so the caller can suppress the
    /// self-originated capture event via `WinCapture::suppress_next`.
    pub fn set_clipboard(&self, data: &ClipData) -> anyhow::Result<u32> {
        set_clipboard(&data.blocks)?;
        Ok(unsafe { GetClipboardSequenceNumber() })
    }
}

impl Paster for WinPaster {
    fn paste(&self, data: &ClipData, mode: PasteMode) -> anyhow::Result<()> {
        // TypeIn overwrites the clipboard although it never reads it back;
        // validate its text-only precondition first so a failed attempt does
        // not also destroy the current clipboard contents (cubic 6311).
        let typed = match mode {
            PasteMode::TypeIn => Some(type_in_units(data)?),
            _ => None,
        };
        set_clipboard(&data.blocks)?;
        match mode {
            PasteMode::SetOnly => Ok(()),
            PasteMode::InjectCtrlV => inject_ctrl_v(),
            PasteMode::TypeIn => send_units(&typed.unwrap()),
        }
    }
}

fn set_clipboard(blocks: &[Block]) -> anyhow::Result<()> {
    if blocks.is_empty() {
        anyhow::bail!("no blocks to paste");
    }
    unsafe {
        // The clipboard is routinely held by other processes (target apps,
        // clipboard managers); retry briefly instead of failing the paste.
        let opened = (0..20).any(|_| match OpenClipboard(None) {
            Ok(()) => true,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(25));
                false
            }
        });
        if !opened {
            anyhow::bail!("OpenClipboard failed after retries (held by another app)");
        }
        let result = (|| -> anyhow::Result<()> {
            EmptyClipboard()?;
            for b in blocks {
                set_block(b)?;
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

/// Set one block. The system takes ownership of the HGLOBAL on success.
unsafe fn set_block(b: &Block) -> anyhow::Result<()> {
    unsafe {
        let (fmt, bytes): (u32, Vec<u8>) = match &b.format {
            Format::UnicodeText => {
                // Stored bytes may already be NUL-terminated; ensure exactly one terminator.
                let units = decode_utf16(&b.bytes);
                let mut bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
                bytes.extend_from_slice(&[0, 0]);
                (13, bytes)
            }
            Format::ImageDib => (8, b.bytes.clone()),
            Format::ImageDibV5 => (17, b.bytes.clone()),
            Format::FileDrop => (15, build_hdrop(&file_list(b)?)),
            Format::Registered(_, name) => {
                // Stored ids are machine-local (X11 hash / foreign machine's
                // registration); 2 would be CF_BITMAP and 0 is invalid.
                // Windows resolves the name to this machine's atom.
                let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                let fmt = RegisterClipboardFormatW(PCWSTR::from_raw(wide.as_ptr()));
                if fmt == 0 {
                    anyhow::bail!("RegisterClipboardFormatW failed for {name:?}");
                }
                (fmt, b.bytes.clone())
            }
        };
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let p = GlobalLock(h);
        if p.is_null() {
            let _ = GlobalFree(h);
            anyhow::bail!("GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
        let _ = GlobalUnlock(h);
        if SetClipboardData(fmt, Some(HANDLE(h.0))).is_err() {
            let _ = GlobalFree(h); // system did not take ownership on failure
            anyhow::bail!("SetClipboardData failed for format {fmt}");
        }
        Ok(())
    }
}

/// Decode stored UTF-16LE bytes, stopping at the first wide NUL.
fn decode_utf16(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .take_while(|c| !(c[0] == 0 && c[1] == 0))
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Parse a stored CF_HDROP block (DROPFILES header + double-NUL-terminated
/// wide path list). Tolerates raw headerless lists too.
fn file_list(b: &Block) -> anyhow::Result<Vec<String>> {
    if b.bytes.len() < 4 {
        anyhow::bail!("file-drop block too short");
    }
    let p_files = u32::from_le_bytes(b.bytes[0..4].try_into()?) as usize;
    let f_wide = b
        .bytes
        .get(16..20)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    // Valid DROPFILES: 20 <= pFiles <= len (fWide selects the encoding).
    // Anything else: a raw headerless WIDE list (our own storage form).
    if (20..=b.bytes.len()).contains(&p_files) {
        let body = &b.bytes[p_files..];
        if f_wide == Some(1) {
            let units: Vec<u16> = body
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            // The list ends at the first empty (double-NUL) segment: bytes
            // after it are allocation padding, not paths.
            let segs: Vec<Vec<u16>> = units
                .split(|&u| u == 0)
                .take_while(|s| !s.is_empty())
                .map(|s| s.to_vec())
                .collect();
            return Ok(decode_segments(&segs));
        }
        if f_wide == Some(0) {
            let segs: Vec<&[u8]> = body
                .split(|&c| c == 0)
                .take_while(|s| !s.is_empty())
                .collect();
            let wide: Vec<Vec<u16>> = segs.iter().map(|s| unsafe { ansi_to_wide(s) }).collect();
            return Ok(decode_segments(&wide));
        }
    }
    let units: Vec<u16> = b
        .bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let list: Vec<String> = units
        .split(|&u| u == 0)
        .take_while(|s| !s.is_empty())
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect();
    if list.is_empty() {
        anyhow::bail!("file-drop block has no paths");
    }
    list.into_iter().map(file_url_to_path).collect()
}

/// Local `file://` URL -> path; plain paths pass through; anything else
/// (http, remote) is rejected (CF_HDROP carries local paths only).
fn file_url_to_path(p: String) -> anyhow::Result<String> {
    if let Some(rest) = p.strip_prefix("file://") {
        let path = rest.strip_prefix("localhost/").unwrap_or(rest);
        let path = percent_decode(path)?;
        let path = if cfg!(windows) {
            path.strip_prefix('/').map(str::to_string).unwrap_or(path)
        } else {
            path
        };
        if path.is_empty() {
            anyhow::bail!("empty file URL");
        }
        return Ok(path);
    }
    if p.contains("://") {
        anyhow::bail!("not a local file path: {p}");
    }
    Ok(p)
}

/// Percent-decode (%41 -> A, %20 -> space); invalid escapes left verbatim.
fn percent_decode(s: &str) -> anyhow::Result<String> {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3])?;
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// ANSI bytes -> UTF-16 via the active Windows code page (for CF_HDROP
/// payloads written with fWide == 0).
unsafe fn ansi_to_wide(bytes: &[u8]) -> Vec<u16> {
    unsafe {
        let n = MultiByteToWideChar(CP_ACP, Default::default(), bytes, None);
        if n <= 0 {
            return Vec::new();
        }
        let mut buf = vec![0u16; n as usize];
        MultiByteToWideChar(CP_ACP, Default::default(), bytes, Some(&mut buf));
        buf
    }
}

/// NUL-separated wide entries -> paths.
fn decode_segments(seg: &[Vec<u16>]) -> Vec<String> {
    seg.iter()
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf16_lossy(s))
        .collect()
}

#[repr(C)]
struct DropFiles {
    p_files: u32,
    pt: [i32; 2],
    f_nc_names: u32,
    f_wide: u32,
}

/// Serialize a CF_HDROP payload: 20-byte DROPFILES header + double-NUL-terminated wide paths.
fn build_hdrop(paths: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let hdr = DropFiles {
        p_files: 20,
        pt: [0, 0],
        f_nc_names: 0,
        f_wide: 1,
    };
    bytes.extend_from_slice(unsafe {
        std::slice::from_raw_parts((&hdr as *const DropFiles).cast::<u8>(), 20)
    });
    for p in paths {
        bytes.extend(p.encode_utf16().flat_map(|u| u.to_le_bytes()));
        bytes.extend_from_slice(&[0, 0]);
    }
    bytes.extend_from_slice(&[0, 0]); // list terminator
    bytes
}

/// Build SendInput INPUT records: key-down + key-up per UTF-16 unit.
/// Surrogate pairs work: each half is sent as its own UNICODE event.
fn build_type_inputs(units: &[u16]) -> Vec<INPUT> {
    let mut out = Vec::with_capacity(units.len() * 2);
    for &u in units {
        for up in [false, true] {
            out.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: u,
                        dwFlags: if up {
                            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                        } else {
                            KEYEVENTF_UNICODE
                        },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
        }
    }
    out
}

/// Type the clip's text as keystrokes (windows that reject Ctrl+V: consoles,
/// elevated apps). Text clips only.
/// Validate the TypeIn precondition and extract the units to type.
fn type_in_units(data: &ClipData) -> anyhow::Result<Vec<u16>> {
    let Some(tb) = data.blocks.iter().find(|b| b.format == Format::UnicodeText) else {
        anyhow::bail!("TypeIn supports text clips only");
    };
    Ok(decode_utf16(&tb.bytes))
}

/// Send the keystroke sequence (no clipboard write).
fn send_units(units: &[u16]) -> anyhow::Result<()> {
    let seq = build_type_inputs(units);
    if seq.is_empty() {
        return Ok(()); // empty string: nothing to type
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    let n = unsafe { SendInput(&seq, std::mem::size_of::<INPUT>() as i32) };
    if n != seq.len() as u32 {
        anyhow::bail!(
            "SendInput blocked (UIPI?): {} of {} events delivered",
            n,
            seq.len()
        );
    }
    Ok(())
}

fn inject_ctrl_v() -> anyhow::Result<()> {
    let down = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let seq = [
        down(VK_CONTROL, false),
        down(VK_V, false),
        down(VK_V, true),
        down(VK_CONTROL, true),
    ];
    std::thread::sleep(std::time::Duration::from_millis(30));
    let n = unsafe { SendInput(&seq, std::mem::size_of::<INPUT>() as i32) };
    if n != seq.len() as u32 {
        anyhow::bail!(
            "SendInput blocked (UIPI?): {} of {} events delivered",
            n,
            seq.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_hdrop, build_type_inputs, decode_utf16, file_list, file_url_to_path};
    use clip_line_core::capture::{Block, Format};

    #[test]
    fn type_inputs_are_down_up_per_unit() {
        let units: Vec<u16> = "héllo🦀".encode_utf16().collect();
        let inputs = build_type_inputs(&units);
        assert_eq!(inputs.len(), units.len() * 2);
        for (i, inp) in inputs.iter().enumerate() {
            let up = i % 2 == 1;
            assert!(
                unsafe { inp.Anonymous.ki.dwFlags.0 }
                    & windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_UNICODE.0
                    != 0
            );
            let keyup = windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_KEYUP.0;
            assert_eq!(unsafe { inp.Anonymous.ki.dwFlags.0 } & keyup != 0, up);
        }
    }

    #[test]
    fn utf16_decode_stops_at_nul() {
        let bytes: Vec<u8> = [0x61u16, 0, 0x62]
            .iter()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(decode_utf16(&bytes), vec![0x61]);
    }

    #[test]
    fn hdrop_round_trips() {
        let paths = vec!["C:\\a b\\c.txt".to_string(), "C:\\d.bin".to_string()];
        let payload = build_hdrop(&paths);
        let files = file_list(&Block {
            format: Format::FileDrop,
            bytes: payload.clone(),
        })
        .unwrap();
        assert_eq!(files, paths);
        // header sanity: pFiles=20, fWide=1
        assert_eq!(u32::from_le_bytes(payload[0..4].try_into().unwrap()), 20);
        assert_eq!(u32::from_le_bytes(payload[16..20].try_into().unwrap()), 1);
    }

    #[test]
    fn file_list_stops_at_double_nul() {
        // cubic 6291: allocation padding after the terminator must not
        // surface as bogus paths.
        let mut units: Vec<u16> = "C:\\real".encode_utf16().collect();
        units.push(0);
        units.push(0); // terminator
        units.extend("C:\\PAD".encode_utf16());
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        let files = file_list(&Block {
            format: Format::FileDrop,
            bytes,
        })
        .unwrap();
        assert_eq!(files, vec!["C:\\real"]);
    }

    #[test]
    fn file_urls_convert_to_local_paths() {
        // cubic 6428: macOS-style file URLs must not ride into CF_HDROP raw.
        assert_eq!(
            file_url_to_path("file:///C:/a%20b.txt".into()).unwrap(),
            "C:/a b.txt"
        );
        assert_eq!(
            file_url_to_path("file://localhost/tmp/x".into()).unwrap(),
            "tmp/x"
        );
        assert!(file_url_to_path("https://x.example".into()).is_err());
        assert_eq!(file_url_to_path("C:\\plain".into()).unwrap(), "C:\\plain");
    }

    #[test]
    fn file_list_tolerates_headerless_payload() {
        // Raw NUL-separated list, no DROPFILES header.
        let mut units: Vec<u16> = "C:\\x".encode_utf16().collect();
        units.push(0);
        units.extend("C:\\y".encode_utf16());
        units.push(0);
        units.push(0);
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        let files = file_list(&Block {
            format: Format::FileDrop,
            bytes,
        })
        .unwrap();
        assert_eq!(files, vec!["C:\\x", "C:\\y"]);
    }
}
