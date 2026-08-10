# Native macOS direct-window capture

`arena-next-macos-capture` is the deliberately small ScreenCaptureKit boundary
for ArenaNext draft recognition. It uses a desktop-independent filter for a
retail Hearthstone window and returns a one-shot BGRA frame. It has no webview,
browser process, file watcher, image matcher, OpenCV dependency, full-desktop
template localization, process-memory access, injection, input synthesis, or
administrator requirement.

## Safety contract

- Only a window whose current ScreenCaptureKit owner bundle ID is a known
  retail Hearthstone ID can be captured. Current builds use
  `unity.Blizzard Entertainment.Hearthstone`; the historical
  `com.blizzard.hearthstone` is retained for compatibility.
- Every capture re-fetches ScreenCaptureKit shareable content and revalidates
  that window ID. A stale or manually constructed `GameWindow` cannot turn
  into arbitrary-app capture.
- `capabilities`, `screen_recording_permission`, discovery, and capture never
  open a macOS prompt. `request_screen_recording_access` is the sole explicit
  prompt API and must be wired to a user gesture.
- The API never captures a display and deliberately has no desktop-capture
  fallback. Its `PlatformCapabilities` reports that state explicitly.
- Frames stay in caller-owned memory. This crate neither persists them nor
  uploads them. Failed debug crops belong behind a separate, opt-in facility.
- Capture is one-shot and synchronous at the boundary. Call it from the
  draft-recognition worker, not the AppKit main thread, and pause it outside
  Arena draft state.
- Before any worker asks for an image, the host must initialize AppKit on the
  macOS main thread. The native overlay host already does this; a minimal CLI
  can call `MacosWindowCapture::initialize_appkit_runtime()` once at startup.

## Why ScreenCaptureKit

The legacy tracker captured an entire display and then localized Hearthstone
with SURF/FLANN. This crate instead asks ScreenCaptureKit for the Hearthstone
window and constructs `initWithDesktopIndependentWindow:` directly. That makes
window movement, multi-monitor layouts, Retina scaling, and native fullscreen
an input to ScreenCaptureKit rather than a computer-vision localization task.

Shareable-content discovery deliberately includes windows across Spaces rather
than asking only for the current on-screen workspace. The latter would hide a
native-fullscreen Hearthstone window when ArenaNext itself is in another Space.
The public result still contains only recognized Hearthstone windows, and the
image path remains a direct-window filter rather than a display fallback.

The crate intentionally does not call deprecated `CGWindowListCreateImage`.
CoreGraphics is used only for its documented Screen Recording permission probe
and request and for reading `CGImage` data returned by ScreenCaptureKit.

## Build and probe

From the ArenaNext workspace root:

```bash
cargo test -p arena-next-macos-capture
cargo check -p arena-next-macos-capture --example capture_probe
cargo run -p arena-next-macos-capture --example capture_probe
```

The last command only prints permission and available Hearthstone-window
metadata. It does not prompt or capture. Opt in independently:

```bash
cargo run -p arena-next-macos-capture --example capture_probe -- --request-permission
cargo run -p arena-next-macos-capture --example capture_probe -- --capture
```

ScreenCaptureKit requires a current macOS release (ArenaNext v0.1 targets
macOS 13 or later). The system may require the packaged application to have
Screen Recording permission; a terminal-launched development binary is a
different TCC identity from the eventual `.app`.

## Integration boundary

The application should use this sequence:

1. Query `capabilities()` and show an explanatory settings state if permission
   is required.
2. In response to a user click only, call `request_screen_recording_access()`.
3. When Arena state enters an unresolved draft offer, call
   `find_hearthstone_windows()` and select the active shareable game window.
4. Initialize AppKit on the main thread before starting capture workers (the
   existing `OverlayHost::new` does this for the native app).
5. On a worker, call `capture_window()` at a bounded rate and send its plain
   `CapturedFrame` to `arena-draft` for crop normalization and confidence
   accumulation.
6. Keep AppKit isolated: convert window/capture coordinates in a dedicated
   tracker before asking the overlay host to update its bounds.

This crate must remain independent of log parsing and scoring. The app layer
owns when capture is useful; the capture layer owns only a safe, direct source.
