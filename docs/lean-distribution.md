# Lean native distribution

ArenaNext's v0.1 distribution target is a macOS `.app` containing one native
`arena-next` Mach-O executable. It uses AppKit and other Apple-provided system
frameworks dynamically. It does not bundle a browser engine or a scripting
runtime.

The supported workspace contains no daemon. The native application hosts the
observer and the AppKit overlay in one process; it keeps a validated local
checkpoint solely to accelerate restart recovery, never a socket service.

## What is and is not in the app

Included:

- compiled Rust observer, parser, reducer, and native overlay;
- a small native icon and application metadata;
- user-selected local settings.

Not included:

- Electron, Chromium, Node.js, a web renderer, Python, Qt, Java, or OpenCV;
- a bundled Hearthstone card-art corpus or the full metadata database;
- Hearthstone binaries, logs, or credentials.

Card metadata is stored outside the bundle at
`~/Library/Application Support/ArenaNext/card-data.json`. The native app reads
a validated local JSON cache; the first release deliberately has no HTTP
updater. This keeps the installer small and leaves room for a later explicit
data-import workflow. Full rendered cards are fetched only when the player
hovers a deck row and then cached under ArenaNext application data; the app
does not prefetch or bundle the complete art corpus.

## Installation path

The first public macOS release will be a host-architecture ZIP containing
`HearthAI.app`, installed by dragging it to Applications. A DMG is optional
presentation polish, not a runtime dependency. The packaging script names and
verifies either an Apple-silicon or Intel executable instead of assuming one.
A later universal binary can follow the same shape. Code signing and
notarization are release requirements once a Developer ID is available; they
do not change the runtime architecture.

For developers, a release candidate is built with:

```bash
scripts/package-macos.sh
```

The script builds an explicit host Rust target with macOS 13 as its Mach-O
deployment target, produces a compact `.app` and ZIP, then verifies the plist,
signature, and executable architecture. The workspace's release profile favors
binary size: size optimization, link time optimization, one code-generation
unit, stripped symbols, and aborting panics. Measurements must always be made
against this release build, never a debug executable.

## Safety boundary

ArenaNext uses only documented local operating-system capabilities:

- file reads and safe tailing of Hearthstone logs;
- an explicit, atomic, backed-up merge of required `log.config` settings;
- user-authorized macOS screen/window capture when draft offers must be read;
- a native overlay window.

It does not inject code, examine Hearthstone memory, terminate or restart the
game, or delete user/Blizzard data. Unsupported platform features must be
shown as unsupported rather than approximated with privileged access.
