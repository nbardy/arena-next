# macOS overlay boundary

The supported macOS application has one small native AppKit overlay window.
It owns no webview and runs alongside the local observer in the same native
process:

```text
Hearthstone logs ──► observer/reducer ──► AppKit overlay
```

`platform/macos/overlay-host` owns the behavior that must remain
AppKit-specific:

| Requirement | Native setting |
| --- | --- |
| Native fullscreen Spaces | `NSWindowCollectionBehavior::FullScreenAuxiliary` |
| All relevant Spaces | `CanJoinAllSpaces` |
| Avoid window-cycle/follow-space behavior | `Stationary | IgnoresCycle` |
| Overlay stacking | `NSStatusWindowLevel` |
| Click-through | `setIgnoresMouseEvents` |
| Hide when Hearthstone loses focus | `NSWorkspace::frontmostApplication` bundle check |

The native host checks frontmost application state without inferring it from a
control window. It must never focus-steal from Hearthstone.

Direct Hearthstone-window capture is implemented separately in
`platform/macos/capture` using ScreenCaptureKit; the recognition worker uses
that boundary without ever falling back to a desktop screenshot. Overlay
repositioning from captured-window coordinates, including multi-monitor
movement, remains a distinct platform integration and acceptance-test concern.
Both concerns stay outside the Rust parser and reducer.
