# HearthAI status popup

The menu-bar item opens a compact native information card rather than a full
settings window. The card is intentionally useful at a glance and remains
independent of the Hearthstone overlay.

## Visual direction

- dark charcoal panel with a thin muted-gold edge;
- one large HearthAI mark at left;
- title and live status at right;
- compact rows with muted labels and bright values;
- no card art, gradients, web UI, or scrolling in the first version;
- target size: roughly 320 × 220 logical points on macOS and equivalent
  physical-pixel bounds on Windows.

## Information hierarchy

```text
[large icon]  HearthAI
              Hearthstone: Running / Not detected

Active deck   Mage · 25/30 observed
Arena mode    Draft / Redraft
Run           3–1 · 4 wins · 2 losses
Pick          14 / 30, or progress unknown

                 [Open overlay] [Quit]
```

The card consumes the platform-neutral `arena-popup::StatusPopupModel` and
`arena_popup::layout()` contract. This is a small pure-Rust crate with no
windowing or widget dependencies, so AppKit and Win32 render the same model
without pulling a GUI framework into the binary:

```rust
struct StatusPopupModel {
    hearthstone: HearthstoneStatus,
    deck_label: Option<String>,
    deck_completeness: Option<String>,
    arena_phase: ArenaPhase,
    pick_progress: Option<Progress>,
    wins: Option<u16>,
    losses: Option<u16>,
}
```

The model also carries overlay visibility/interaction state and exposes the
stable commands `ShowOverlay`, `HideOverlay`, `ToggleInteraction`,
`OpenSettings`, and `Quit`. Native hosts should dispatch these commands from
their local controls and must not invent platform-specific labels. The shared
layout is 320 × 220 logical points with five information rows and three action
button rectangles; each host scales those rectangles using its backing scale.

The macOS implementation should use an `NSPanel` or `NSPopover` anchored to
the status item. Windows should use a small layered popup anchored to the
notification-area icon. Both should reuse the same model, layout, and command
names. Linux may use the same contract with a capability-limited host.

The current native seam is implemented without a widget runtime:

* `arena-popup::StatusPopupModel` is the shared payload and layout contract.
* macOS `OverlayHost` owns a retained 320 × 220 `NSPanel` and exposes
  `show_popup`, `hide_popup`, and `update_popup`.
* The status menu includes a `Status` command that emits `ShowPopup`.
* Windows exposes the same `set_popup` payload API behind its native shell;
  layered Win32 painting and notification-area anchoring remain isolated in
  the Windows adapter and are not compiled into macOS builds.

This keeps styling and event dispatch native while making content, labels, and
geometry portable. No Electron, WebView, Qt, or general-purpose UI framework
is required.

## Candidate icon assets

The generated exploration sheet is in `tmp/imagegen/icon-grid-alpha.png`.
Four transparent candidates were cropped and normalized to 512 px, with
18 px and 36 px menu-bar variants:

- `assets/icons/candidates/diamond-faceted.png`
- `assets/icons/candidates/starburst.png`
- `assets/icons/candidates/arena-helm.png`
- `assets/icons/candidates/diamond-ring.png`

The selected mark is the transparent Arena Helm candidate:

- `assets/arena-next-icon.png` is the 512 px bundle/Finder source;
- `assets/icons/arena-next-menu-18.png` and `arena-next-menu-36.png` are the
  menu-bar density variants;
- `platform/macos/overlay-host` embeds the 36 px variant in the native status
  item, with the `◈` glyph retained as a decode-failure fallback.

These files have alpha corners and do not contain the chroma-key background.
