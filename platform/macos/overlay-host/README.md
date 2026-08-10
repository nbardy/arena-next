# Native macOS overlay host

`arena-next-macos-overlay` is a small AppKit-only foundation for the
ArenaNext overlay. It deliberately contains no webview, JavaScript runtime,
browser bundle, screen capture, log parsing, Hearthstone process inspection,
or file-system access.

It creates a borderless `NSPanel` with these macOS behaviors:

- `NSStatusWindowLevel` for an always-on-top overlay;
- `CanJoinAllSpaces | FullScreenAuxiliary | Stationary | IgnoresCycle` so it
  can appear above native fullscreen Hearthstone across Spaces without joining
  Cmd-` window cycling;
- `NonactivatingPanel`, click-through enabled by default, and
  `orderFrontRegardless` rather than `makeKeyAndOrderFront`, so rendering does
  not activate ArenaNext or take keyboard focus from Hearthstone;
- transparent background and an updateable native `NSTextField`.
- a retained native `NSStatusItem` labelled `Arena` in the macOS menu bar;
- a lower-right native `NSButton` affordance labelled `ArenaNext`. The panel
  remains click-through by default; callers can opt into interaction with
  `set_interactive(true)` (or `set_click_through(false)`) and update the
  label with `set_action_button_title`.
- a main-thread `run_with_tick` loop built on the system `NSRunLoop`, not an
  async runtime or a browser timer;
- a narrow `frontmost_bundle_id` / `hearthstone_is_frontmost` helper using
  `NSWorkspace` bundle IDs only.

## Build and test

This is a root-workspace member and a deliberately narrow platform leaf. Only
the native `arena-next` application depends on it; parser, reducer, and
observer crates remain AppKit-free.

```bash
cargo test -p arena-next-macos-overlay
cargo check -p arena-next-macos-overlay --example overlay_demo
cargo run -p arena-next-macos-overlay --example overlay_demo
```

The demo opens a small click-through overlay and then enters AppKit's event
loop. Every 500 ms it checks whether retail Hearthstone is the frontmost app,
updates its text model, and hides the overlay when it is inactive. Quit it
from the terminal with `Ctrl-C` while developing.

## Integration contract

`OverlayHost` is main-thread-only. The observer/parser must never mutate an
AppKit object from its log-tail thread; it should post a complete
`OverlayModel` to the native application's main loop, then call
`update_model(&model)` there.

For the single-binary v0.1 application, use the built-in tick loop instead of
an extra scheduler runtime:

```rust
overlay.run_with_tick(Duration::from_millis(500), |host| {
    let snapshot = observer.latest_snapshot(); // cheap, non-blocking read
    host.update_model(&render(snapshot))?;
    if hearthstone_is_frontmost()? {
        host.show()?;
    } else {
        host.hide()?;
    }
    Ok(TickControl::Continue)
})?;
```

The callback runs immediately and then approximately once per interval. Keep
it short: it should render an already-available observer snapshot, not tail
files, recognize card art, or perform network work on the AppKit main thread.
Return `TickControl::Stop` for a clean exit.

`OverlayBounds` uses AppKit global **points**, with the AppKit lower-left
origin. A future `GameWindowProvider` owns conversion from a window capture's
pixel coordinates, Retina backing scale, and its top-left coordinate system.
Keeping that conversion outside this crate is what lets the overlay host stay
small and testable.

For a packaged `.app`, set `LSUIElement` to `true` in `Info.plist` as well as
using the runtime `Accessory` activation policy. That prevents an ordinary
Dock application from appearing while the overlay is running.

## Explicit limits

This component does not locate, inspect, inject into, or control Hearthstone.
It does not capture a screen or game window. macOS may still require the user
to grant Screen Recording permission to the separate capture component.

`hearthstone_is_frontmost` only compares the currently frontmost macOS bundle
ID with the current Unity (`unity.Blizzard Entertainment.Hearthstone`) or
legacy (`com.blizzard.hearthstone`) retail identifier; it does not read game
memory or attach to the game process.

Fullscreen placement should be manually exercised on every supported macOS
release and display arrangement. The relevant AppKit flags are encoded here,
but a final acceptance test must cover native fullscreen, Spaces, Retina, and
multiple monitors on real hardware.
