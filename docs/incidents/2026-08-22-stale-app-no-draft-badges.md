# 2026-08-22: stale ArenaNext app launched instead of HearthAI

## User-visible failure

Hearthstone was running an active Druid Arena draft, but no card-score badges
were visible. A small deck-state panel eventually appeared in the lower-left,
showing that log parsing was alive, while the three draft cards had no score
overlays.

The visible offer at the time of final verification was:

- Natural Causes
- Ghost Writer
- Yesterloc

## Root cause

Two app bundles with the same product lineage were installed:

- `/Applications/ArenaNext.app`: obsolete July 20 development build
- `/Applications/HearthAI.app`: current packaged build (`0.1.4`)

The obsolete `ArenaNext.app` was the process that had been launched. It could
read the current draft and create the generic deck panel, but it was not the
packaged HearthAI build we intended to test.

The installed HearthAI executable was verified byte-for-byte against
`dist/HearthAI.app/Contents/MacOS/arena-next`. The raw Cargo executable has a
different hash because app packaging adds the Mach-O code signature.

After replacing the running process, the first HearthAI instance still did not
show badges. A clean diagnostic restart captured the live offer successfully,
resolved all three card names, and created three visible 276 x 84 AppKit score
panels at the expected card positions. The one OCR correction involved was
`Yesterloo` to `Yesterloc`; the existing unique, bounded edit-distance resolver
handled it correctly. No source patch was required.

## Recovery performed

1. Terminated the process whose executable was
   `/Applications/ArenaNext.app/Contents/MacOS/arena-next`.
2. Moved the obsolete app, release ZIPs `0.1.1` through `0.1.3`, and three old
   package backups into
   `~/.Trash/arena-next-obsolete-20260822T145500KST/`.
3. Retained `/Applications/HearthAI.app`, `dist/HearthAI.app`, and
   `dist/HearthAI-0.1.4-macos-arm64.zip`.
4. Started `/Applications/HearthAI.app/Contents/MacOS/arena-next` cleanly.
5. Verified a live attachment to
   `/Applications/Hearthstone/Logs/Hearthstone_2026_08_22_14_22_38` using a
   validated observer checkpoint.
6. Verified Screen Recording permission, direct Hearthstone-window capture,
   OCR output, and the three onscreen score-panel windows.

## Useful diagnostics

Read current state without opening another overlay:

```bash
/Applications/HearthAI.app/Contents/MacOS/arena-next --once
/Applications/HearthAI.app/Contents/MacOS/arena-next doctor --json
```

Test the live title-band OCR without storing a full Hearthstone frame:

```bash
/Applications/HearthAI.app/Contents/MacOS/arena-next --read-offer --json
```

The rolling OCR audit is written to:

```text
~/Library/Application Support/ArenaNext/ocr-audit/
```

The observer/attachment log is:

```text
~/Library/Logs/ArenaNext/app.log
```

## Follow-up hazards

- The app package is version `0.1.4`, but the Rust executable reports
  `ArenaNext 0.1.0`. Align Cargo and bundle versions so build identity is
  unambiguous during incidents.
- The rename from ArenaNext to HearthAI left a launchable legacy app in
  `/Applications`. Future installation/release instructions should detect and
  remove or explicitly warn about the legacy bundle.
- Finder launches discard stdout/stderr. Capture offer-recognition failures in
  `app.log` or the status popup so a retry failure is not silent.
- The menu and overlay still render the internal title `ArenaNext`; decide
  whether that is intentional or complete the product rename consistently.

