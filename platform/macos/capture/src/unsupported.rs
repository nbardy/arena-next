use crate::{
    CaptureError, CaptureOptions, CaptureProvider, CapturedFrame, FeatureAvailability, GameWindow,
    GameWindowProvider, PlatformCapabilities, ScreenRecordingPermission,
};

/// A compile-time placeholder so consumers can report a clear capability state
/// on non-macOS builds instead of pretending direct window capture works.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacosWindowCapture;

impl MacosWindowCapture {
    pub const fn new() -> Self {
        Self
    }

    pub fn capabilities(&self) -> PlatformCapabilities {
        unavailable_capabilities()
    }

    pub fn screen_recording_permission(&self) -> ScreenRecordingPermission {
        ScreenRecordingPermission::Required
    }

    /// Never opens a prompt on an unsupported operating system.
    pub fn request_screen_recording_access(&self) -> ScreenRecordingPermission {
        ScreenRecordingPermission::Required
    }
}

impl GameWindowProvider for MacosWindowCapture {
    fn capabilities(&self) -> PlatformCapabilities {
        self.capabilities()
    }

    fn find_hearthstone_windows(&self) -> Result<Vec<GameWindow>, CaptureError> {
        Err(CaptureError::UnsupportedPlatform(
            "ScreenCaptureKit direct-window capture requires macOS",
        ))
    }
}

impl CaptureProvider for MacosWindowCapture {
    fn capture_window(
        &self,
        _window: &GameWindow,
        _options: CaptureOptions,
    ) -> Result<CapturedFrame, CaptureError> {
        Err(CaptureError::UnsupportedPlatform(
            "ScreenCaptureKit direct-window capture requires macOS",
        ))
    }
}

fn unavailable_capabilities() -> PlatformCapabilities {
    const REASON: &str = "ScreenCaptureKit direct-window capture requires macOS";
    PlatformCapabilities {
        hearthstone_window_discovery: FeatureAvailability::Unsupported(REASON),
        direct_window_capture: FeatureAvailability::Unsupported(REASON),
        full_desktop_capture: FeatureAvailability::DisabledByDesign(
            "ArenaNext never falls back to full-desktop capture",
        ),
        process_memory_inspection: FeatureAvailability::DisabledByDesign(
            "ArenaNext never inspects Hearthstone process memory",
        ),
        input_injection: FeatureAvailability::DisabledByDesign(
            "ArenaNext never injects input into Hearthstone",
        ),
        requires_screen_recording_permission: false,
    }
}
