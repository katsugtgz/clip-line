//! NSPasteboard write + CGEventPost (Cmd+V injection). InjectCtrlV needs the
//! Accessibility permission (user story 9): on failure we return an Err
//! telling the user to grant it.

use clip_line_core::capture::{Block, Format};
use clip_line_core::paste::{ClipData, PasteMode, Paster};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSData, NSString};

pub struct MacPaster;

// CoreGraphics event functions, declared directly (same pattern as the win
// adapter's GlobalFree): links against the CoreGraphics framework.
#[allow(non_snake_case)]
mod cg {
    #[repr(transparent)]
    #[derive(Clone, Copy)]
    pub struct CGEventTapLocation(pub u32);

    #[repr(transparent)]
    #[derive(Clone, Copy)]
    pub struct CGKeyCode(pub u16);

    // kCGSessionEventTap / kCGEventSourceStateCombinedSessionState
    pub const SESSION_EVENT_TAP: CGEventTapLocation = CGEventTapLocation(0);
    pub const COMBINED_SESSION_STATE: u32 = 0;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        pub fn CGEventCreateKeyboardEvent(
            source: *const std::ffi::c_void,
            virtual_key: CGKeyCode,
            key_down: bool,
        ) -> *mut std::ffi::c_void;
        pub fn CGEventPost(tap: CGEventTapLocation, event: *mut std::ffi::c_void);
        pub fn CGEventSetFlags(event: *mut std::ffi::c_void, flags: u64);
        pub fn CGEventSourceCreate(state_id: u32) -> *mut std::ffi::c_void;
        pub fn CFRelease(cf: *mut std::ffi::c_void);
        // ApplicationServices: no-arg trust check (does not prompt)
        pub fn AXIsProcessTrusted() -> bool;
    }

    /// True when this process may post CGEvents (Accessibility granted).
    /// CGEventPost has no return value, so this is the only way to detect a
    /// missing permission instead of reporting a silent no-op as success.
    pub fn ax_trusted() -> bool {
        unsafe { AXIsProcessTrusted() }
    }
}

// kVK_ANSI_V = 0x09; kCGEventFlagMaskCommand = 1<<20.
const KVK_ANSI_V: cg::CGKeyCode = cg::CGKeyCode(0x09);
const FLAG_MASK_COMMAND: u64 = 1 << 20;

impl MacPaster {
    /// Puts `data` on the pasteboard (SetOnly semantics) and returns the new
    /// changeCount, so the caller can suppress the self-originated capture
    /// event via `MacCapture::suppress_next`.
    pub fn set_clipboard(&self, data: &ClipData) -> anyhow::Result<isize> {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            write_blocks(&pb, &data.blocks)?;
            Ok(pb.changeCount())
        }
    }
}

impl Paster for MacPaster {
    fn paste(&self, data: &ClipData, mode: PasteMode) -> anyhow::Result<()> {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            write_blocks(&pb, &data.blocks)?;
            match mode {
                PasteMode::SetOnly => Ok(()),
                PasteMode::InjectCtrlV => inject_cmd_v(),
                // ponytail: TypeIn via CGEventCreateKeyboardEvent per UTF-16
                // unit when macOS TypeIn demand appears (SPEC phase 4).
                PasteMode::TypeIn => Err(anyhow::anyhow!(
                    "TypeIn not implemented on macOS yet (SPEC phase 4)"
                )),
            }
        }
    }
}

/// Pure mapping: Block -> (UTI string, payload bytes). Unit-test surface.
pub fn block_to_ns_type(b: &Block) -> Option<(String, Vec<u8>)> {
    match &b.format {
        Format::UnicodeText => {
            // public.utf8-plain-text expects UTF-8 bytes; the stored block is
            // UTF-16LE (may contain embedded NULs) — decode first.
            Some((
                "public.utf8-plain-text".into(),
                String::from_utf16_lossy(&decode_utf16(&b.bytes)).into_bytes(),
            ))
        }
        Format::ImageDib | Format::ImageDibV5 => {
            // DIB payloads are Windows-format; macOS paste needs PNG. No
            // DIB->PNG decoder in-tree; emit as-is under a private UTI so
            // round-trip through our own store still works.
            // ponytail: DIB/PNG conversion when paste fidelity matters.
            Some(("com.clip-line.dib".into(), b.bytes.clone()))
        }
        Format::FileDrop => {
            let urls = crate::capture::file_urls_from_bytes(&b.bytes);
            if urls.is_empty() {
                return None;
            }
            Some((
                "public.file-url".into(),
                crate::capture::file_urls_to_bytes(&urls),
            ))
        }
        Format::Registered(_, name) => match name.as_str() {
            "PNG" => Some(("public.png".into(), b.bytes.clone())),
            "TIFF" => Some(("public.tiff".into(), b.bytes.clone())),
            other => Some((other.to_string(), b.bytes.clone())),
        },
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

unsafe fn write_blocks(pb: &NSPasteboard, blocks: &[Block]) -> anyhow::Result<()> {
    unsafe {
        if blocks.is_empty() {
            anyhow::bail!("no blocks to paste");
        }
        // Build and validate everything BEFORE clearing: a failed conversion
        // must not destroy the user's current clipboard (cubic 6331).
        let mut file_items: Vec<objc2::rc::Retained<NSPasteboardItem>> = Vec::new();
        let text_item = NSPasteboardItem::new();
        let mut have_text = false;
        for b in blocks {
            let Some((uti, bytes)) = block_to_ns_type(b) else {
                continue;
            };
            if uti == "public.file-url" {
                for url in crate::capture::file_urls_from_bytes(&bytes) {
                    file_items.push(file_item(&url));
                }
            } else {
                // All companion representations ride ONE item: apps that read
                // the first logical item lose alternates otherwise (cubic 6349).
                let uti_s = NSString::from_str(&uti);
                let data = NSData::with_bytes(&bytes);
                if !text_item.setData_forType(&data, &uti_s) {
                    anyhow::bail!("setData_forType failed for {uti}");
                }
                have_text = true;
            }
        }
        if !have_text && file_items.is_empty() {
            anyhow::bail!("no block had a macOS pasteboard representation");
        }
        let mut objects: Vec<objc2::rc::Retained<ProtocolObject<dyn NSPasteboardWriting>>> =
            Vec::new();
        if have_text {
            objects.push(ProtocolObject::from_retained(text_item));
        }
        objects.extend(file_items.into_iter().map(ProtocolObject::from_retained));
        pb.clearContents();
        if !pb.writeObjects(&NSArray::from_retained_slice(&objects)) {
            anyhow::bail!("writeObjects failed");
        }
        Ok(())
    }
}

/// One file item: the URL string under public.file-url.
unsafe fn file_item(url: &str) -> objc2::rc::Retained<NSPasteboardItem> {
    let item = NSPasteboardItem::new();
    item.setString_forType(
        &NSString::from_str(url),
        &NSString::from_str("public.file-url"),
    );
    item
}

fn inject_cmd_v() -> anyhow::Result<()> {
    unsafe {
        if !cg::ax_trusted() {
            anyhow::bail!(
                "clipLine needs the Accessibility permission to inject Cmd+V (System Settings > Privacy & Security > Accessibility) and retry"
            );
        }
        let source = cg::CGEventSourceCreate(cg::COMBINED_SESSION_STATE);
        let down = cg::CGEventCreateKeyboardEvent(source, KVK_ANSI_V, true);
        if down.is_null() {
            release(source);
            anyhow::bail!(
                "CGEventCreateKeyboardEvent failed: grant clipLine Accessibility permission (System Settings > Privacy & Security > Accessibility) and retry"
            );
        }
        let up = cg::CGEventCreateKeyboardEvent(source, KVK_ANSI_V, false);
        cg::CGEventSetFlags(down, FLAG_MASK_COMMAND);
        cg::CGEventPost(cg::SESSION_EVENT_TAP, down);
        cg::CGEventPost(cg::SESSION_EVENT_TAP, up);
        release(source);
        release(down);
        release(up);
        Ok(())
    }
}

unsafe fn release(p: *mut std::ffi::c_void) {
    unsafe {
        if !p.is_null() {
            cg::CFRelease(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{block_to_ns_type, decode_utf16};
    use clip_line_core::capture::{Block, Format};

    #[test]
    fn text_block_maps_to_utf8_bytes() {
        // cubic 5888: the UTI is utf8-plain-text, so the payload must be
        // UTF-8 (stored block bytes are UTF-16LE).
        let b = Block {
            format: Format::UnicodeText,
            bytes: "h\u{e9}"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect(),
        };
        let (uti, bytes) = block_to_ns_type(&b).unwrap();
        assert_eq!(uti, "public.utf8-plain-text");
        assert_eq!(bytes, "h\u{e9}".as_bytes());
    }

    #[test]
    fn file_drop_block_round_trips_url_list() {
        let urls = ["file:///a.txt", "file:///b.txt"];
        let list: Vec<String> = urls.iter().map(|s| s.to_string()).collect();
        let bytes = crate::capture::file_urls_to_bytes(&list);
        let b = Block {
            format: Format::FileDrop,
            bytes: bytes.clone(),
        };
        let (uti, out) = block_to_ns_type(&b).unwrap();
        assert_eq!(uti, "public.file-url");
        assert_eq!(out, bytes, "paster re-serializes the same storage form");
    }

    #[test]
    fn registered_blocks_map_by_name() {
        let b = Block {
            format: Format::Registered(8, "PNG".into()),
            bytes: vec![1],
        };
        assert_eq!(block_to_ns_type(&b).unwrap().0, "public.png");
        let b = Block {
            format: Format::Registered(0, "com.custom.x".into()),
            bytes: vec![1],
        };
        assert_eq!(block_to_ns_type(&b).unwrap().0, "com.custom.x");
    }

    #[test]
    fn dib_blocks_take_a_private_uti() {
        for f in [Format::ImageDib, Format::ImageDibV5] {
            let b = Block {
                format: f,
                bytes: vec![1],
            };
            assert_eq!(block_to_ns_type(&b).unwrap().0, "com.clip-line.dib");
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
}
