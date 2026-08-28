#![deny(unsafe_op_in_unsafe_fn)]

//! Direct, user-consented macOS window capture for ArenaNext.
//!
//! This is intentionally a narrow platform boundary. It obtains a
//! ScreenCaptureKit filter for a **retail Hearthstone window only**, captures
//! that one window as a one-shot BGRA frame, and returns plain Rust data to
//! the caller. It does not inspect game memory, inject input or code, request
//! elevated privileges, capture a display, locate a window from a desktop
//! screenshot or write image files. Its optional sidebar reader runs Apple's
//! on-device Vision OCR only on the cropped Hearthstone deck panel.
//!
//! A caller should use [`MacosWindowCapture::capabilities`] before showing a
//! capture-dependent UI. That probe never opens a system prompt. Only an
//! explicit user action may call [`MacosWindowCapture::request_screen_recording_access`].
//!
//! Capture is synchronous at this boundary because it is a small one-shot
//! operation. It waits for ScreenCaptureKit's completion handler, so invoke
//! it from the draft-recognition worker rather than the AppKit main thread.

use std::{error::Error, fmt, time::Duration};

/// The bundle identifier reported by current macOS retail Hearthstone builds.
///
/// ArenaNext observed this identifier from the current `Hearthstone.app`
/// `Info.plist`; the older tracker used a different historical identifier.
pub const HEARTHSTONE_BUNDLE_ID: &str = "unity.Blizzard Entertainment.Hearthstone";

/// Older retail macOS Hearthstone identifier retained for compatible window
/// discovery while users transition between game-client generations.
pub const LEGACY_HEARTHSTONE_BUNDLE_ID: &str = "com.blizzard.hearthstone";

/// Whether a bundle ID belongs to a supported retail Hearthstone client.
pub fn is_hearthstone_bundle_id(bundle_id: &str) -> bool {
    matches!(
        bundle_id,
        HEARTHSTONE_BUNDLE_ID | LEGACY_HEARTHSTONE_BUNDLE_ID
    )
}

/// A stable macOS window-server identifier.
///
/// It is intentionally not a process ID and cannot be used to inspect a
/// process. [`CaptureProvider::capture_window`] validates it against a fresh
/// ScreenCaptureKit Hearthstone window listing before every capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u32);

impl fmt::Display for WindowId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A rectangle as reported by `SCWindow.frame`, measured in ScreenCaptureKit
/// display-coordinate **points**.
///
/// This is deliberately not converted to an AppKit overlay rectangle here:
/// the overlay host owns its own global coordinate system and a future window
/// tracker must account for the active display and backing scale explicitly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowFrame {
    pub x_points: f64,
    pub y_points: f64,
    pub width_points: f64,
    pub height_points: f64,
}

impl WindowFrame {
    /// Returns `true` for a finite, non-empty frame that can be captured.
    pub fn is_valid(self) -> bool {
        self.x_points.is_finite()
            && self.y_points.is_finite()
            && self.width_points.is_finite()
            && self.height_points.is_finite()
            && self.width_points > 0.0
            && self.height_points > 0.0
    }
}

/// Metadata for a user-visible Hearthstone window.
///
/// The information comes from ScreenCaptureKit's user-consented shareable
/// content list. It does not read process memory or attach to the game.
#[derive(Clone, Debug, PartialEq)]
pub struct GameWindow {
    pub id: WindowId,
    pub owner_bundle_id: String,
    pub owner_name: String,
    pub title: Option<String>,
    pub frame: WindowFrame,
    pub window_layer: i64,
    pub on_screen: bool,
    pub active: bool,
}

impl GameWindow {
    /// Whether this object identifies the retail Hearthstone application.
    pub fn is_hearthstone(&self) -> bool {
        is_hearthstone_bundle_id(&self.owner_bundle_id)
    }

    /// The window's reported area in ScreenCaptureKit points.
    ///
    /// This is useful for choosing the primary game surface over Hearthstone's
    /// small helper/title windows. It is not a pixel count and must not be
    /// mixed with a capture frame's physical dimensions.
    pub fn area_points(&self) -> f64 {
        self.frame.width_points * self.frame.height_points
    }
}

/// Whether an individual platform feature can be used right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeatureAvailability {
    /// The capability can be used without a further system prompt.
    Available,
    /// The user must grant macOS Screen Recording access in response to an
    /// explicit application action or in System Settings.
    PermissionRequired,
    /// ArenaNext intentionally does not implement this behavior.
    DisabledByDesign(&'static str),
    /// The current operating system cannot provide this capability.
    Unsupported(&'static str),
}

impl FeatureAvailability {
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Capture-related behavior that the current platform implementation exposes.
///
/// The negative entries are intentional safety guarantees, not temporary
/// omissions. Consumers should use these values rather than assuming a
/// fallback from direct-window capture to a desktop screenshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformCapabilities {
    pub hearthstone_window_discovery: FeatureAvailability,
    pub direct_window_capture: FeatureAvailability,
    pub full_desktop_capture: FeatureAvailability,
    pub process_memory_inspection: FeatureAvailability,
    pub input_injection: FeatureAvailability,
    pub requires_screen_recording_permission: bool,
}

/// The result of checking or explicitly requesting Screen Recording access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenRecordingPermission {
    Granted,
    Required,
}

impl ScreenRecordingPermission {
    pub const fn is_granted(self) -> bool {
        matches!(self, Self::Granted)
    }
}

/// Limits for a one-shot capture request.
///
/// The default bounds constrain temporary memory while keeping enough detail
/// for draft-card crops. The resulting image preserves aspect ratio; it may
/// be smaller than the limit on a low-resolution display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureOptions {
    pub timeout: Duration,
    pub max_width_px: u32,
    pub max_height_px: u32,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3),
            max_width_px: 2_560,
            max_height_px: 1_440,
        }
    }
}

impl CaptureOptions {
    fn validate(self) -> Result<(), CaptureError> {
        if self.timeout.is_zero() {
            return Err(CaptureError::InvalidOptions(
                "capture timeout must be greater than zero",
            ));
        }
        if self.max_width_px == 0 || self.max_height_px == 0 {
            return Err(CaptureError::InvalidOptions(
                "capture dimensions must be greater than zero",
            ));
        }
        Ok(())
    }
}

/// The source format used by a [`CapturedFrame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// ScreenCaptureKit was configured for 8-bit BGRA, one byte per channel.
    Bgra8,
}

/// A direct-window image in CPU memory.
///
/// `pixels` is row-major and can include row padding. The capture component
/// deliberately leaves normalization, cropping, hashing, and card matching to
/// `arena-draft`; it does not keep a frame cache or write captures to disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedFrame {
    pub window_id: WindowId,
    pub width_px: u32,
    pub height_px: u32,
    pub bytes_per_row: usize,
    pub pixel_format: PixelFormat,
    pub pixels: Vec<u8>,
}

/// A Vision OCR result in normalized image coordinates with a lower-left origin.
#[derive(Clone, Debug, PartialEq)]
pub struct RecognizedText {
    pub text: String,
    pub confidence: f32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One direct-window frame plus text read from Hearthstone's deck sidebar.
#[derive(Clone, Debug, PartialEq)]
pub struct DeckSidebarCapture {
    pub frame: CapturedFrame,
    pub text: Vec<RecognizedText>,
}

/// One direct-window frame plus OCR results for the three current draft cards.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftOfferTextCapture {
    pub frame: CapturedFrame,
    pub header: Vec<RecognizedText>,
    pub offers: [Vec<RecognizedText>; 3],
}

impl CapturedFrame {
    /// Returns the BGRA bytes for one row, excluding any padding after it.
    pub fn row(&self, row_index: u32) -> Option<&[u8]> {
        let row_bytes = self.width_px.checked_mul(4)? as usize;
        if row_index >= self.height_px || row_bytes > self.bytes_per_row {
            return None;
        }
        let start = self.bytes_per_row.checked_mul(row_index as usize)?;
        let end = start.checked_add(row_bytes)?;
        self.pixels.get(start..end)
    }
}

/// Errors surfaced by the user-consented capture boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    PermissionRequired,
    UnsupportedPlatform(&'static str),
    NoHearthstoneWindow,
    WindowNotFound(WindowId),
    TimedOut(Duration),
    InvalidOptions(&'static str),
    InvalidFrame(&'static str),
    AppKitInitialization(&'static str),
    ScreenCaptureKit(String),
    TextRecognition(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionRequired => formatter.write_str(
                "macOS Screen Recording permission is required for direct Hearthstone-window capture",
            ),
            Self::UnsupportedPlatform(reason) => write!(formatter, "unsupported platform: {reason}"),
            Self::NoHearthstoneWindow => formatter.write_str(
                "no shareable retail Hearthstone window is currently available for capture",
            ),
            Self::WindowNotFound(window_id) => {
                write!(formatter, "Hearthstone window {window_id} is no longer available")
            }
            Self::TimedOut(timeout) => {
                write!(formatter, "ScreenCaptureKit did not finish within {timeout:?}")
            }
            Self::InvalidOptions(reason) => write!(formatter, "invalid capture options: {reason}"),
            Self::InvalidFrame(reason) => write!(formatter, "invalid direct-window frame: {reason}"),
            Self::AppKitInitialization(reason) => {
                write!(formatter, "AppKit capture-runtime initialization failed: {reason}")
            }
            Self::ScreenCaptureKit(reason) => write!(formatter, "ScreenCaptureKit error: {reason}"),
            Self::TextRecognition(reason) => {
                write!(formatter, "Vision text recognition error: {reason}")
            }
        }
    }
}

impl Error for CaptureError {}

/// Locates only the supported retail Hearthstone window(s).
pub trait GameWindowProvider {
    fn capabilities(&self) -> PlatformCapabilities;
    fn find_hearthstone_windows(&self) -> Result<Vec<GameWindow>, CaptureError>;
}

/// Captures a currently shareable Hearthstone window without a desktop
/// fallback. Implementations must re-validate the target before capture.
pub trait CaptureProvider: GameWindowProvider {
    fn capture_window(
        &self,
        window: &GameWindow,
        options: CaptureOptions,
    ) -> Result<CapturedFrame, CaptureError>;
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(target_os = "macos"))]
mod unsupported;

#[cfg(target_os = "macos")]
pub use macos::{MINIMUM_SUPPORTED_MACOS, MacosWindowCapture};
#[cfg(not(target_os = "macos"))]
pub use unsupported::MacosWindowCapture;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rows_exclude_padding() {
        let frame = CapturedFrame {
            window_id: WindowId(42),
            width_px: 2,
            height_px: 2,
            bytes_per_row: 12,
            pixel_format: PixelFormat::Bgra8,
            pixels: vec![
                0, 1, 2, 3, 4, 5, 6, 7, 99, 99, 99, 99, 8, 9, 10, 11, 12, 13, 14, 15, 99, 99, 99,
                99,
            ],
        };

        assert_eq!(frame.row(0), Some(&[0, 1, 2, 3, 4, 5, 6, 7][..]));
        assert_eq!(frame.row(1), Some(&[8, 9, 10, 11, 12, 13, 14, 15][..]));
        assert_eq!(frame.row(2), None);
    }

    #[test]
    fn capture_options_reject_zero_limits() {
        let error = CaptureOptions {
            max_width_px: 0,
            ..CaptureOptions::default()
        }
        .validate()
        .expect_err("zero width must fail");
        assert!(matches!(error, CaptureError::InvalidOptions(_)));
    }

    #[test]
    fn hearthstone_identity_uses_bundle_id() {
        let window = GameWindow {
            id: WindowId(1),
            owner_bundle_id: HEARTHSTONE_BUNDLE_ID.to_owned(),
            owner_name: "Hearthstone".to_owned(),
            title: None,
            frame: WindowFrame {
                x_points: 0.0,
                y_points: 0.0,
                width_points: 100.0,
                height_points: 100.0,
            },
            window_layer: 0,
            on_screen: true,
            active: true,
        };

        assert!(window.is_hearthstone());
        assert!(is_hearthstone_bundle_id(LEGACY_HEARTHSTONE_BUNDLE_ID));
        assert!(!is_hearthstone_bundle_id("com.example.not-hearthstone"));
    }

    #[test]
    fn game_window_area_uses_point_coordinates() {
        let window = GameWindow {
            id: WindowId(1),
            owner_bundle_id: HEARTHSTONE_BUNDLE_ID.to_owned(),
            owner_name: "Hearthstone".to_owned(),
            title: None,
            frame: WindowFrame {
                x_points: 0.0,
                y_points: 0.0,
                width_points: 500.0,
                height_points: 300.0,
            },
            window_layer: 0,
            on_screen: true,
            active: true,
        };

        assert_eq!(window.area_points(), 150_000.0);
    }
}
