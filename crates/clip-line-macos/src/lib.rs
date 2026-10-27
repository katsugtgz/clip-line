//! macOS adapters behind cfg(target_os = "macos"); empty stubs elsewhere so the
//! workspace always compiles (SPEC phase 3: CI matrix stays green on Windows).

#[cfg(target_os = "macos")]
pub mod capture;
#[cfg(target_os = "macos")]
pub mod paste;
#[cfg(target_os = "macos")]
pub use capture::MacCapture;
#[cfg(target_os = "macos")]
pub use paste::MacPaster;
