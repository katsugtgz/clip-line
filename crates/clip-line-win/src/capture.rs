//! AddClipboardFormatListener + WM_CLIPBOARDUPDATE on a hidden message window.

use clip_line_core::capture::{Block, Capture, ClipEvent, Format};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardFormatNameW, OpenClipboard, RemoveClipboardFormatListener,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetWindowTextW,
    GetWindowThreadProcessId, MSG, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLIPBOARDUPDATE,
};
use windows::core::PCWSTR;

/// Clipboard formats backed by global memory (HGLOBAL); everything else
/// (GDI/metafile handles) is skipped.
const HGLOBAL_FORMATS: &[u32] = &[1, 7, 8, 13, 15, 16, 17];

pub struct WinCapture {
    hwnd: isize,
}

impl WinCapture {
    pub fn new() -> anyhow::Result<Self> {
        unsafe {
            let class_name: Vec<u16> = "clipLineListener\0".encode_utf16().collect();
            let class_name = PCWSTR(class_name.as_ptr());
            let wc = windows::Win32::UI::WindowsAndMessaging::WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                lpszClassName: class_name,
                ..Default::default()
            };
            windows::Win32::UI::WindowsAndMessaging::RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                None,
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )?;
            if let Err(e) = AddClipboardFormatListener(hwnd) {
                let _ = DestroyWindow(hwnd);
                return Err(e.into());
            }
            Ok(WinCapture {
                hwnd: hwnd.0 as isize,
            })
        }
    }
}

impl Drop for WinCapture {
    fn drop(&mut self) {
        unsafe {
            let hwnd = HWND(self.hwnd as *mut _);
            let _ = RemoveClipboardFormatListener(hwnd);
            let _ = DestroyWindow(hwnd);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

impl Capture for WinCapture {
    fn next_event(&mut self) -> anyhow::Result<ClipEvent> {
        unsafe {
            let mut msg = MSG::default();
            loop {
                let r = GetMessageW(&mut msg, Some(HWND(self.hwnd as *mut _)), 0, 0);
                if r.0 <= 0 {
                    anyhow::bail!("message loop ended: {}", r.0);
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
                if msg.message == WM_CLIPBOARDUPDATE {
                    // Self-writes are filtered by the app's shared SuppressSlot
                    // (payload identity), not a race-prone arm-after-write
                    // sequence (cubic 6379).
                    // EmptyClipboard or foreign formats we can't read leave
                    // zero blocks — that state carries no clip; keep waiting.
                    let ev = read_clipboard()?;
                    if !ev.blocks.is_empty() {
                        return Ok(ev);
                    }
                }
            }
        }
    }
}

unsafe fn read_clipboard() -> anyhow::Result<ClipEvent> {
    unsafe {
        let (app, title) = foreground_app();
        let mut blocks = Vec::new();
        // Cross-process contention: another app may hold the clipboard for a
        // few ms. Bounded retry (SPEC capture: adapters own retries); only a
        // persistent failure falls back to the empty (skip) event.
        // Cross-process contention: another app may hold the clipboard for
        // a while. Consume-and-drop loses the ONLY notification, so keep
        // retrying within a generous window before giving up (cubic 6583).
        let mut opened = false;
        for _ in 0..200 {
            if OpenClipboard(None).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !opened {
            return Ok(ClipEvent {
                blocks,
                source_app: app,
                source_title: title,
            });
        }
        let mut fmt = EnumClipboardFormats(0);
        while fmt != 0 {
            if let Some(b) = read_format(fmt) {
                blocks.push(b);
            }
            fmt = EnumClipboardFormats(fmt);
        }
        let _ = CloseClipboard();
        Ok(ClipEvent {
            blocks,
            source_app: app,
            source_title: title,
        })
    }
}

/// True when the format's data is an HGLOBAL we can GlobalLock.
fn is_hglobal_format(fmt: u32) -> bool {
    HGLOBAL_FORMATS.contains(&fmt) || fmt >= 0xC000 // registered formats
}

unsafe fn read_format(fmt: u32) -> Option<Block> {
    unsafe {
        let format = match fmt {
            13 => Format::UnicodeText,
            8 => Format::ImageDib,
            17 => Format::ImageDibV5, // ponytail: distinct id; V5 header differs from BITMAPINFOHEADER
            15 => Format::FileDrop,
            other => {
                if !is_hglobal_format(other) {
                    return None; // GDI/metafile handles: not global memory
                }
                let mut name = [0u16; 128];
                let n = GetClipboardFormatNameW(other, &mut name);
                if n == 0 {
                    return None;
                }
                let s = String::from_utf16_lossy(&name[..n as usize]);
                Format::Registered(other, s)
            }
        };
        let handle: HANDLE = GetClipboardData(fmt).ok()?; // delayed-render sources can fail here; skip not crash
        let h = HGLOBAL(handle.0);
        let p = GlobalLock(h);
        if p.is_null() {
            return None;
        }
        let alloc = GlobalSize(h);
        // Producers can over-allocate: for NUL-terminated formats stop at the
        // first wide NUL pair instead of copying the whole allocation.
        let len = match format {
            Format::UnicodeText => {
                let units = std::slice::from_raw_parts(p as *const u16, alloc / 2);
                let n = units
                    .iter()
                    .position(|&u| u == 0)
                    .map(|i| (i + 1) * 2)
                    .unwrap_or(alloc);
                n.min(alloc)
            }
            _ => alloc,
        };
        let bytes = std::slice::from_raw_parts(p as *const u8, len).to_vec();
        let _ = GlobalUnlock(h);
        Some(Block { format, bytes })
    }
}

/// Foreground window's process name (basename, no `.exe`) and window title.
/// Unknown steps degrade to "" — never block capture on lookup failure.
unsafe fn foreground_app() -> (String, String) {
    unsafe {
        let app = foreground_process_name().unwrap_or_default();
        let title = foreground_title().unwrap_or_default();
        (app, title)
    }
}

unsafe fn foreground_process_name() -> Option<String> {
    unsafe {
        let hwnd = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if r.is_err() {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let base = path.rsplit(['/', '\\']).next().unwrap_or(&path);
        Some(base.strip_suffix(".exe").unwrap_or(base).to_string())
    }
}

unsafe fn foreground_title() -> Option<String> {
    unsafe {
        let hwnd = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..n as usize]))
    }
}
