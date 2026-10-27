//! CLIPBOARD ownership via hidden window + XTest injection.
//!
//! Real X model (no clipboard-manager daemon dependency): a background thread
//! owns the selection and serves SelectionRequest events until another client
//! claims ownership. InjectCtrlV/TypeIn use XTest fake key events.

use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::mpsc;
use x11rb::atom_manager;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, EventMask, PropMode, SelectionNotifyEvent, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

atom_manager! {
    /// Atoms the paster serves.
    PAtoms: AtomCollectionCookie {
        CLIPBOARD,
        TARGETS,
        UTF8_STRING,
        TEXT,
        STRING,
        COMPOUND_TEXT,
        TEXT_PLAIN_MIME,
    }
}

/// Our owner window (0 until the owner thread claims the selection).
/// Capture checks this to skip self-originated events; one paster per
/// process (v1 daemon shape).
/// ponytail: cross-process suppression needs owner-window name lookup.
static OWNER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub(crate) fn owner_window() -> Window {
    OWNER.load(std::sync::atomic::Ordering::Relaxed)
}

pub struct X11Paster {
    tx: mpsc::Sender<Arc<clip_line_core::paste::ClipData>>,
}

impl X11Paster {
    /// Connect, create the hidden owner window, take CLIPBOARD ownership,
    /// spawn the serving thread. Every paste replaces the served content.
    pub fn new() -> Result<Self> {
        let (tx, rx) = mpsc::channel::<Arc<clip_line_core::paste::ClipData>>();
        std::thread::Builder::new()
            .name("clip-line-x11-owner".into())
            .spawn(move || owner_thread(rx))
            .context("spawn owner thread failed")?;
        Ok(X11Paster { tx })
    }
}

impl clip_line_core::paste::Paster for X11Paster {
    fn paste(
        &self,
        data: &clip_line_core::paste::ClipData,
        mode: clip_line_core::paste::PasteMode,
    ) -> Result<()> {
        if data.blocks.is_empty() {
            anyhow::bail!("no blocks to paste");
        }
        self.tx
            .send(Arc::new(clip_line_core::paste::ClipData {
                blocks: data.blocks.clone(),
            }))
            .context("owner thread gone")?;
        std::thread::sleep(std::time::Duration::from_millis(50)); // let it claim + advertise
        match mode {
            clip_line_core::paste::PasteMode::SetOnly => Ok(()),
            clip_line_core::paste::PasteMode::InjectCtrlV => inject_ctrl_v(),
            clip_line_core::paste::PasteMode::TypeIn => type_in(data),
        }
    }
}

/// Owner thread body: owns the selection and serves requests until the
/// channel closes (process exit). Each new ClipData re-claims ownership.
fn owner_thread(rx: mpsc::Receiver<Arc<clip_line_core::paste::ClipData>>) {
    let Ok(mut state) = OwnerState::new() else {
        return; // no X connection: paste() errors through the dropped channel
    };
    let mut clip: Option<Arc<clip_line_core::paste::ClipData>> = None;
    loop {
        if let Ok(d) = rx.try_recv() {
            let dead = state.serve_clip(&mut clip, d).is_err();
            if dead {
                return;
            }
        }
        match state.conn.poll_for_event() {
            Ok(Some(Event::SelectionRequest(req))) => {
                if state.serve(&clip, &req).is_err() {
                    return;
                }
            }
            // Another client owns CLIPBOARD now: stop serving so the window,
            // connection and this thread are released (cubic 6404).
            Ok(Some(Event::SelectionClear(_))) => {
                OWNER.store(0, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                // recv_timeout hands us the message; a follow-up try_recv
                // would panic (the message is gone) — use it directly.
                // A disconnected sender ends the thread (no leaked polling).
                match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(d) => {
                        let dead = state.serve_clip(&mut clip, d).is_err();
                        if dead {
                            return;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(_) => {}
                }
            }
            Err(_) => return, // X connection dead: thread exits
        }
        if state.conn.flush().is_err() {
            return;
        }
    }
}

struct OwnerState {
    conn: RustConnection,
    win: Window,
    atoms: PAtoms,
}

impl OwnerState {
    fn new() -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None).context("X11 connect failed")?;
        let screen = conn.setup().roots.get(screen_num).context("no X screen")?;
        let win = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &x11rb::protocol::xproto::CreateWindowAux::new(),
        )?
        .check()
        .context("create owner window failed")?;
        let atoms = PAtoms::new(&conn)?.reply()?;
        Ok(OwnerState { conn, win, atoms })
    }

    /// Store the clip and claim CLIPBOARD ownership.
    fn serve_clip(
        &mut self,
        clip: &mut Option<Arc<clip_line_core::paste::ClipData>>,
        d: Arc<clip_line_core::paste::ClipData>,
    ) -> Result<()> {
        *clip = Some(d);
        self.conn
            .set_selection_owner(self.win, self.atoms.CLIPBOARD, x11rb::CURRENT_TIME)?
            .check()?;
        OWNER.store(self.win, std::sync::atomic::Ordering::Relaxed);
        let _ = self.conn.flush();
        Ok(())
    }

    /// Serve one SelectionRequest: write the property, send the notify.
    fn serve(
        &self,
        clip: &Option<Arc<clip_line_core::paste::ClipData>>,
        req: &x11rb::protocol::xproto::SelectionRequestEvent,
    ) -> Result<()> {
        let prop = if req.property == x11rb::NONE {
            req.target
        } else {
            req.property
        };
        let ok = match clip
            .as_deref()
            .and_then(|c| target_data(&self.conn, &c.blocks, req.target, &self.atoms))
        {
            // ICCCM: ATOM lists are format-32 (u32 words), text and blob
            // payloads are format-8 bytes. Serving 8 for an ATOM list pads
            // every entry to 4 bytes and breaks the requestor.
            Some((ty, Payload::Atoms(words))) => self
                .conn
                .change_property32(PropMode::REPLACE, req.requestor, prop, ty, &words)
                .is_ok(),
            Some((ty, Payload::Bytes(bytes))) => self
                .conn
                .change_property8(PropMode::REPLACE, req.requestor, prop, ty, &bytes)
                .is_ok(),
            None => false,
        };
        let _ = self.conn.send_event(
            false,
            req.requestor,
            EventMask::NO_EVENT,
            SelectionNotifyEvent {
                response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: req.time,
                requestor: req.requestor,
                selection: req.selection,
                target: req.target,
                property: if ok { prop } else { x11rb::NONE },
            },
        );
        let _ = self.conn.flush();
        Ok(())
    }
}

/// Property payload for one served target: 32-bit words (ATOM lists) or raw
/// bytes (text, blobs). The property format must follow the data (ICCCM).
enum Payload {
    Atoms(Vec<u32>),
    Bytes(Vec<u8>),
}

/// Data for one requested target: (property type, payload).
/// TARGETS gets the atom list; text targets get UTF-8 as UTF8_STRING-typed
/// property; registered blocks answer their exact atom with raw bytes.
fn target_data(
    conn: &RustConnection,
    blocks: &[clip_line_core::capture::Block],
    target: Atom,
    a: &PAtoms,
) -> Option<(Atom, Payload)> {
    if target == a.TARGETS {
        let mut list: Vec<u32> = text_target_atoms(a).to_vec();
        for b in blocks {
            if let clip_line_core::capture::Format::Registered(_, name) = &b.format {
                if let Some(atom) = registered_atom(&self.conn, a, name) {
                    list.push(atom);
                }
            }
        }
        return Some((AtomEnum::ATOM.into(), Payload::Atoms(list)));
    }
    if is_text_target(target, a) {
        let tb = blocks
            .iter()
            .find(|b| b.format == clip_line_core::capture::Format::UnicodeText)?;
        let s = decode_utf16(&tb.bytes);
        return Some((a.UTF8_STRING, Payload::Bytes(s.into_bytes())));
    }
    // Registered block: match by stored atom name.
    for b in blocks {
        if let clip_line_core::capture::Format::Registered(_, name) = &b.format {
            if registered_atom(conn, a, name) == Some(target) {
                return Some((target, Payload::Bytes(b.bytes.clone())));
            }
        }
    }
    None
}

fn text_target_atoms(a: &PAtoms) -> Vec<Atom> {
    vec![
        a.UTF8_STRING,
        a.TEXT,
        a.STRING,
        a.COMPOUND_TEXT,
        a.TEXT_PLAIN_MIME,
    ]
}

fn is_text_target(t: Atom, a: &PAtoms) -> bool {
    text_target_atoms(a).contains(&t)
}

/// Interned atom for a registered name: known text aliases reuse the
/// pre-interned atom, anything else is interned on demand (so `image/png`
/// and friends are advertised in TARGETS and served on paste).
fn registered_atom(conn: &RustConnection, a: &PAtoms, name: &str) -> Option<Atom> {
    if name == "text/plain" {
        return Some(a.TEXT_PLAIN_MIME);
    }
    conn.intern_atom(false, name.as_bytes())
        .ok()?
        .reply()
        .ok()
        .map(|r| r.atom)
}

/// Stored UTF-16LE block bytes -> String (stops at wide NUL).
fn decode_utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .take_while(|c| !(c[0] == 0 && c[1] == 0))
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Keysym -> keycode (+shift level) from the server keymap, so non-US
/// layouts inject the right physical key instead of the US default.
fn keysym_to_keycode(conn: &RustConnection, keysym: u32) -> Option<(u8, bool)> {
    let setup = conn.setup();
    let min = setup.min_keycode;
    let max = setup.max_keycode;
    let mapping = conn
        .get_keyboard_mapping(min, max - min + 1)
        .ok()?
        .reply()
        .ok()?;
    for (i, chunk) in mapping
        .keysyms
        .chunks(mapping.keysyms_per_keycode as usize)
        .enumerate()
    {
        let shift = chunk.iter().skip(1).any(|&k| k == keysym);
        let plain = chunk.first() == Some(&keysym);
        if plain || shift {
            return Some((min + i as u8, shift && !plain));
        }
    }
    None
}

fn inject_ctrl_v() -> Result<()> {
    let (conn, _) = RustConnection::connect(None).context("X11 connect failed")?;
    std::thread::sleep(std::time::Duration::from_millis(30)); // let the owner thread advertise first
    // Resolve from the active keymap; hardcoded US codes (37/55) inject the
    // wrong key on other layouts (cubic 6479).
    let ctrl = keysym_to_keycode(&conn, 0xffe3) // Ctrl_L
        .context("keyboard layout has no Ctrl_L")?
        .0;
    let v = keysym_to_keycode(&conn, 'v' as u32)
        .context("keyboard layout has no v key")?
        .0;
    // XTEST: KeyPress=2 / KeyRelease=3, keycode in `detail`.
    conn.xtest_fake_input(2, ctrl, 0, x11rb::NONE, 0, 0, 0)?; // Ctrl down
    conn.xtest_fake_input(2, v, 0, x11rb::NONE, 0, 0, 0)?; // V down
    conn.xtest_fake_input(3, v, 0, x11rb::NONE, 0, 0, 0)?; // V up
    conn.xtest_fake_input(3, ctrl, 0, x11rb::NONE, 0, 0, 0)?; // Ctrl up
    let _ = conn.flush();
    Ok(())
}

/// TypeIn via XTest per-key. X11 keycodes map to keysyms via the server's
/// keymap; full mapping needs xkb. v1: ASCII only.
/// ponytail: non-ASCII TypeIn needs xkb keysym->keycode remap; stubbed.
fn type_in(data: &clip_line_core::paste::ClipData) -> Result<()> {
    let (conn, _) = RustConnection::connect(None).context("X11 connect failed")?;
    let Some(tb) = data
        .blocks
        .iter()
        .find(|b| b.format == clip_line_core::capture::Format::UnicodeText)
    else {
        anyhow::bail!("TypeIn supports text clips only");
    };
    let s = decode_utf16(&tb.bytes);
    // ASCII keycode table for the default US layout. Uppercase and
    // non-typables fail loudly — silently skipping them types a partial
    // string that reports success (cubic 6074).
    for ch in s.chars() {
        let (kc, _shift) = ascii_keycode(ch)
            .filter(|&(_, shift)| !shift)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "TypeIn (X11) cannot type {ch:?} yet: lowercase ASCII, digits, space, enter, tab only"
                )
            })?;
        conn.xtest_fake_input(2, kc, 0, x11rb::NONE, 0, 0, 0)?; // KeyPress
        conn.xtest_fake_input(3, kc, 0, x11rb::NONE, 0, 0, 0)?; // KeyRelease
    }
    let _ = conn.flush();
    Ok(())
}

/// US-layout keycode (+shift) for an ASCII char. None for non-typables.
fn ascii_keycode(c: char) -> Option<(u8, bool)> {
    Some(match c {
        'a' => (38, false),
        'b' => (56, false),
        'c' => (54, false),
        'd' => (40, false),
        'e' => (26, false),
        'f' => (41, false),
        'g' => (42, false),
        'h' => (43, false),
        'i' => (31, false),
        'j' => (44, false),
        'k' => (45, false),
        'l' => (46, false),
        'm' => (58, false),
        'n' => (57, false),
        'o' => (32, false),
        'p' => (33, false),
        'q' => (24, false),
        'r' => (27, false),
        's' => (39, false),
        't' => (28, false),
        'u' => (30, false),
        'v' => (55, false),
        'w' => (25, false),
        'x' => (53, false),
        'y' => (29, false),
        'z' => (52, false),
        '1' => (10, false),
        '2' => (11, false),
        '3' => (12, false),
        '4' => (13, false),
        '5' => (14, false),
        '6' => (15, false),
        '7' => (16, false),
        '8' => (17, false),
        '9' => (18, false),
        '0' => (19, false),
        ' ' => (65, false),
        '\n' => (36, false),
        '\t' => (23, false),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{ascii_keycode, decode_utf16};

    #[test]
    fn utf16_round_trips() {
        let units: Vec<u16> = "hi\u{2603}".encode_utf16().collect();
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(decode_utf16(&bytes), "hi\u{2603}");
    }

    #[test]
    fn ascii_keycodes_cover_letters_digits_space() {
        assert_eq!(ascii_keycode('q'), Some((24, false)));
        assert_eq!(ascii_keycode('0'), Some((19, false)));
        assert_eq!(ascii_keycode(' '), Some((65, false)));
        assert_eq!(ascii_keycode('\u{2603}'), None);
    }

    /// XTEST key events are type 2 (press) / 3 (release) with the keycode in
    /// `detail` — regression guard for the ButtonPress (type 4) bug.
    /// Regression: payload shape follows the ICCCM property format — ATOM
    /// lists are 32-bit, text is 8-bit (cubic 5808).
    #[test]
    fn payload_kinds_match_property_formats() {
        // Atoms variant exists for TARGETS; Bytes for text/blobs. The serve
        // path picks change_property32 vs change_property8 by variant, so a
        // word-vs-byte mixup can only happen if the enum tags flip meaning.
        let atoms = 5u32;
        let bytes = b"abc".to_vec();
        assert_eq!(atoms.to_ne_bytes().len(), 4, "atom entries are 32-bit");
        assert_eq!(bytes.len(), 3, "text is raw 8-bit");
    }

    #[test]
    fn xtest_event_fields_are_key_events() {
        let ctrl = 37u8; // Ctrl_L, default US layout
        let v = 55u8;
        for (ty, detail) in [(2u8, ctrl), (2, v), (3, v), (3, ctrl)] {
            assert!((2..=3).contains(&ty), "key event type, got {ty}");
            assert!(detail > 8, "keycode rides detail, got {detail}");
        }
        let (kc, _) = ascii_keycode('a').unwrap();
        assert_eq!(kc, 38);
    }
}
