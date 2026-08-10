# HearthAI

HearthAI is a macOS-first, GPL-2.0-only Hearthstone companion designed
to ship as a small native application—not an Electron app and not a bundled
browser runtime. Its release path is one native Rust executable linked only to
macOS system frameworks, with a compact AppKit overlay.

This repository is a fresh implementation. `legacy-arena-tracker` is kept
beside it only as a GPL-2.0 reference checkout; ArenaNext never loads or
executes files from that checkout.

## Current vertical slice

`arena-next` is the user-facing native application. It inspects Hearthstone
paths and logging configuration without writing by default, reconstructs a
live Arena deck, resolves cards from a versioned local JSON cache, and renders
through an AppKit overlay. Given an explicit local fingerprint catalog, it
also performs confidence-gated direct-window draft matching and can display
local ratings. It is the only supported executable; the old socket daemon is
retired outside the release workspace.

```bash
# Supply a local rules manifest when a partial log replay needs a known deck
# size. ArenaNext never assumes 30 slots without this explicit input.
cargo run -p arena-next -- replay fixtures/logs/sample-arena-session \
  --cards fixtures/card-data/sample-cards.json \
  --arena-rules fixtures/arena-rules/sample-season.json

# Read-only checks and safe, explicit log-config workflow.
cargo run -p arena-next -- doctor
cargo run -p arena-next -- logging inspect
cargo run -p arena-next -- logging diff

# This is the only user-facing command that can change log.config. It creates
# a backup and asks the user—not ArenaNext—to restart Hearthstone if needed.
cargo run -p arena-next -- --enable-logging

# Inspect direct Hearthstone-window capture status without a macOS prompt.
cargo run -p arena-next -- --capture-status

# Explicitly test one direct Hearthstone-window capture in memory. It never
# captures a display or writes an image file.
cargo run -p arena-next -- --capture-window

# Read the visible committed deck sidebar with local Apple Vision OCR and
# resolve names against the local Hearthstone card catalog.
cargo run -p arena-next -- --read-deck

# Emit deterministic deck facts and candidate deltas; this never calls an AI.
cargo run -p arena-next -- analyze \
  --logs fixtures/logs/sample-arena-session \
  --cards fixtures/card-data/sample-cards.json \
  --arena-rules fixtures/arena-rules/sample-season.json \
  --analysis-facts fixtures/analysis/sample-facts.json \
  --ratings fixtures/ratings/sample-ratings.json \
  --offer CS2_029 --offer EX1_116
```

The replay and analysis examples are fixture-based and work without
Hearthstone installed. `doctor` discovers the live macOS paths and reports
exactly what is missing instead of changing `log.config` implicitly.

Normal live startup does not replay a giant historical session just to draw a
current deck. It reverse-searches `Arena.log` for the newest authoritative
deck snapshot, parses that small span and a bounded Arena suffix, and starts
gameplay logs at EOF. `arena-next replay` is the explicit complete-history
path.

The macOS shell includes a compact `◈` menu-bar status item with show/hide,
interaction, and quit commands. The overlay also has a lower-right native
action button; the overlay remains click-through until interaction is enabled
from the status menu. The shared model is platform-neutral, while Windows has
an isolated native shell adapter using the same status/overlay contract.

## Lean distribution contract

- No Electron, Chromium, Node, Java, Python, Qt, or OpenCV runtime is shipped.
- Card metadata is refreshable into Application Support; it is not embedded in
  the executable or installer. The first release accepts validated local JSON
  imports; it does not ship an HTTP updater.
- The app only reads Hearthstone logs and captures a user-approved Hearthstone
  window when draft recognition needs it. It never captures the desktop,
  injects into Hearthstone, reads game memory, asks for root, deletes logs, or
  restarts the game.
- `log.config` is inspected by default. A change requires explicit command
  confirmation, creates a backup, and is written atomically.

See [the distribution notes](docs/lean-distribution.md) for the exact release
shape and the current development-to-release transition. See
[draft-recognition.md](docs/draft-recognition.md) for the optional local
fingerprint and rating workflow, and
[contextual-draft-analysis.md](docs/contextual-draft-analysis.md) for the
deterministic-first optional AI analyst design.
See [arena-rules.md](docs/arena-rules.md) for the local expected-deck-size
manifest used to report incomplete decks truthfully.

## License

ArenaNext is licensed under GPL-2.0-only. See `LICENSE` before distributing a
build. The implementation may adapt GPL-2.0 Arena Tracker behavior and code;
distribution of such derivatives must include corresponding source under GPL
version 2.
