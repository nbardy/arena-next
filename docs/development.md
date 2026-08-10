# Development workflow

ArenaNext is native Rust end to end. There is no Node, Electron, browser, or
webview development environment to install.

## Headless engine

```bash
# Deterministic fixture replay
cargo run -p arena-next -- replay fixtures/logs/sample-arena-session \
  --cards fixtures/card-data/sample-cards.json

# Read current macOS client state without mutating configuration
cargo run -p arena-next -- doctor

# Inspect, preview, or explicitly restore only an ArenaNext-created backup.
cargo run -p arena-next -- logging inspect
cargo run -p arena-next -- logging diff

# Render the real native app model without opening an overlay window
cargo run -p arena-next -- --once

# Check the direct-window draft-capture boundary without prompting macOS.
cargo run -p arena-next -- --capture-status

# Explicitly make one in-memory capture of a current Hearthstone window.
# This never falls back to a display screenshot and never writes an image.
cargo run -p arena-next -- --capture-window

# Open the native AppKit overlay without touching Hearthstone
cargo run -p arena-next -- --demo

# Run the parser/reducer/observer fixtures
cargo test --workspace
```

The supported workspace has one application process and no socket daemon. A
normal overlay restart writes and then validates a compact local checkpoint;
if validation fails, it safely performs the current-deck tail resync instead.
Use the explicit `replay` command when a complete historical log replay is
actually required.

## Native package

```bash
scripts/package-macos.sh
```

The script creates a small ad-hoc-signed development `.app` plus a ZIP in
`dist/`. It refuses to overwrite an existing package unless
`ARENA_NEXT_REPLACE=1` is explicitly set. A public release supplies
`ARENA_NEXT_SIGN_IDENTITY` and completes Developer-ID notarization separately.

A change to `log.config` is the only normal reason to restart Hearthstone;
parser changes are validated through fixture replay and then recover against
the active current-deck snapshot. They do not require a historical replay
during ordinary live startup.

## Troubleshooting

- The overlay says "Hearthstone logs stalled": the game's component log
  writers stopped advancing (see the failure mode in `log-format.md`). The
  session's logs cannot be re-animated; restart Hearthstone. The checkpoint
  and prior snapshot survive the restart. Rotation should prevent this from
  occurring at all; `~/Library/Logs/ArenaNext/app.log` records every rotation
  (`rotated <component> from N bytes, retained M`). A rotation is not an
  error — the observer re-attaches the retained tail automatically.
- The running app writes a small client log to
  `~/Library/Logs/ArenaNext/app.log` (attach method, staleness transitions,
  observer errors). The app otherwise prints only to stdout/stderr, which
  disappear when launched from Finder, so check that file when a failure is
  silent.
- `doctor` reports `logStaleness` with the newest component write age and the
  `attachMethod` explains why history was or was not read.
- A rotation rewrites the log in place, which bumps its mtime. A rotated file
  therefore briefly reads as "live" for a dead session; the next
  `LOG_STALENESS_THRESHOLD` gap restores the stale classification. This is
  cosmetic and self-correcting.
- The overlay must follow a new Hearthstone session when one starts. A
  bounded-history recovery used to replace the session-following observer with
  a fixed full-replay attach, pinning the tracker (and rotation) to the stale
  session (regression 2026-08-06). The recovery path now uses
  `attach_full_replay_and_follow_discovered_with_expected_deck_slots`; if the
  overlay ever stops tracking a fresh session, check `app.log`'s `attached`
  line for which session it chose.

## Before distribution

- Build and test the signed native `HearthAI.app` package.
- Test native Hearthstone fullscreen, all Spaces, Retina, and multiple
  monitors on supported macOS releases.
- Capture and sanitize one complete normal draft session and opt-in screenshot
  crops.
- Review third-party card/rating/art data terms separately from the GPL code.
