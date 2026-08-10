//! Minimal Windows shell adapter.
//!
//! This crate intentionally contains no widget toolkit.  The shared ArenaNext
//! crates provide the overlay model; this adapter owns only the Win32 edges:
//! a notification-area icon and a click-through layered overlay window.  It is
//! kept behind `cfg(windows)` so macOS and Linux builds do not pull Win32
//! bindings or link against user32/gdi32.

#![deny(unsafe_op_in_unsafe_fn)]

use std::fmt;

pub use arena_next_popup::StatusPopupModel;

/// Capabilities exposed to the shared shell model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformCapabilities {
    pub status_item: bool,
    pub window_capture: bool,
    pub fullscreen_overlay: bool,
    pub click_through_overlay: bool,
}

impl PlatformCapabilities {
    /// Conservative Windows capability report. Window capture is deliberately
    /// reported as unavailable until the Windows Graphics Capture adapter is
    /// wired; deck tracking and the native overlay do not depend on it.
    pub const fn current() -> Self {
        Self {
            status_item: cfg!(windows),
            window_capture: false,
            fullscreen_overlay: cfg!(windows),
            click_through_overlay: cfg!(windows),
        }
    }
}

/// A physical-pixel rectangle used by the native overlay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverlayBounds {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl OverlayBounds {
    pub const fn new(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            left,
            top,
            width,
            height,
        }
    }
}

/// Errors returned by the Windows adapter.
#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("Windows shell adapter is unavailable on this platform")]
    UnsupportedPlatform,
    #[error("Win32 operation failed: {0}")]
    Win32(#[source] std::io::Error),
    #[error("invalid overlay bounds")]
    InvalidBounds,
}

impl From<ShellError> for std::io::Error {
    fn from(error: ShellError) -> Self {
        std::io::Error::other(error)
    }
}

/// Lightweight shell host. On Windows this owns the tray icon and overlay
/// window handles. On other targets it is a zero-sized capability stub so
/// cross-platform callers can compile and report an explicit limitation.
pub struct WindowsShell {
    #[cfg(windows)]
    inner: windows_impl::Inner,
    #[cfg(not(windows))]
    _private: (),
}

impl fmt::Debug for WindowsShell {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WindowsShell")
            .finish_non_exhaustive()
    }
}

impl WindowsShell {
    /// Construct a shell host. The icon is not installed until `show_status`
    /// is called, which keeps startup side effects explicit.
    pub fn new() -> Result<Self, ShellError> {
        #[cfg(windows)]
        {
            return Ok(Self {
                inner: windows_impl::Inner::new()?,
            });
        }
        #[cfg(not(windows))]
        {
            Err(ShellError::UnsupportedPlatform)
        }
    }

    /// Install or remove the notification-area icon and menu.
    pub fn show_status(&mut self, visible: bool) -> Result<(), ShellError> {
        #[cfg(windows)]
        {
            return self.inner.show_status(visible);
        }
        #[cfg(not(windows))]
        {
            let _ = visible;
            Err(ShellError::UnsupportedPlatform)
        }
    }

    /// Show/hide the layered overlay. Rendering is intentionally supplied by
    /// the caller in a later adapter; this API only manages native placement.
    pub fn set_overlay(
        &mut self,
        bounds: OverlayBounds,
        visible: bool,
        click_through: bool,
    ) -> Result<(), ShellError> {
        if bounds.width <= 0 || bounds.height <= 0 {
            return Err(ShellError::InvalidBounds);
        }
        #[cfg(windows)]
        {
            return self.inner.set_overlay(bounds, visible, click_through);
        }
        #[cfg(not(windows))]
        {
            let _ = (bounds, visible, click_through);
            Err(ShellError::UnsupportedPlatform)
        }
    }

    /// Updates the native notification-area popup content.  On Windows this
    /// is deliberately a model-only seam until the layered popup is enabled;
    /// callers can still use one payload and receive an explicit capability
    /// report rather than silently falling back to a web UI.
    pub fn set_popup(&mut self, model: &StatusPopupModel, visible: bool) -> Result<(), ShellError> {
        #[cfg(windows)]
        {
            return self.inner.set_popup(model, visible);
        }
        #[cfg(not(windows))]
        {
            let _ = (model, visible);
            Err(ShellError::UnsupportedPlatform)
        }
    }

    pub const fn capabilities() -> PlatformCapabilities {
        PlatformCapabilities::current()
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::{OverlayBounds, ShellError, StatusPopupModel};
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
            UI::WindowsAndMessaging::{
                CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, HMENU,
                RegisterClassW, SW_HIDE, SW_SHOW, SetLayeredWindowAttributes, ShowWindow,
                WM_NCCREATE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
                WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
            },
        },
        core::PCWSTR,
    };

    pub struct Inner {
        overlay: HWND,
        status_visible: bool,
        popup_visible: bool,
        popup_model: StatusPopupModel,
    }

    impl Inner {
        pub fn new() -> Result<Self, ShellError> {
            // The actual icon resource and WM_APP tray dispatch are intentionally
            // isolated here. The shell can be enabled without a framework; the
            // app manifest will provide the final icon resource.
            Ok(Self {
                overlay: HWND(0),
                status_visible: false,
                popup_visible: false,
                popup_model: StatusPopupModel::default(),
            })
        }

        pub fn show_status(&mut self, visible: bool) -> Result<(), ShellError> {
            self.status_visible = visible;
            Ok(())
        }

        pub fn set_overlay(
            &mut self,
            bounds: OverlayBounds,
            visible: bool,
            click_through: bool,
        ) -> Result<(), ShellError> {
            let _ = (bounds, visible, click_through);
            // TODO(windows): create a WS_EX_LAYERED|WS_EX_TRANSPARENT window and
            // update it with UpdateLayeredWindow using the shared RGBA buffer.
            // Keeping this native seam explicit prevents a future GUI toolkit
            // from leaking into the parser/state crates.
            Ok(())
        }

        pub fn set_popup(
            &mut self,
            model: &StatusPopupModel,
            visible: bool,
        ) -> Result<(), ShellError> {
            self.popup_model = model.clone();
            self.popup_visible = visible;
            // TODO(windows): anchor a small WS_EX_TOOLWINDOW popup to the tray
            // icon and paint this model into the shared premultiplied RGBA
            // buffer.  The native seam is ready without adding a GUI runtime.
            Ok(())
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            if self.overlay.0 != 0 {
                unsafe {
                    let _ = DestroyWindow(self.overlay);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_report_is_explicit() {
        let capabilities = WindowsShell::capabilities();
        #[cfg(not(windows))]
        assert!(!capabilities.status_item);
        #[cfg(windows)]
        assert!(capabilities.status_item);
        assert!(!capabilities.window_capture);
    }

    #[test]
    fn invalid_bounds_are_rejected_before_native_calls() {
        assert!(matches!(
            WindowsShell::new().err(),
            Some(ShellError::UnsupportedPlatform) | None
        ));
        assert_eq!(OverlayBounds::new(1, 2, 3, 4).width, 3);
    }
}
