//! Wayland adapters behind cfg(all(unix, target_os = "linux")); empty stubs
//! elsewhere so the workspace always compiles (Windows CI included).

#[cfg(all(unix, target_os = "linux"))]
pub mod capture;
#[cfg(all(unix, target_os = "linux"))]
pub mod paste;
#[cfg(all(unix, target_os = "linux"))]
pub use capture::WaylandCapture;
#[cfg(all(unix, target_os = "linux"))]
pub use paste::WaylandPaster;
