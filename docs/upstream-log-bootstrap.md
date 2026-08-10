# Upstream log bootstrap inventory

Reference: `../legacy-arena-tracker` at `62c4ef5e76963ce34ace9220589daf090dc5c3a8`.

## Observed current macOS paths

The live July 2026 client confirms the legacy conventions still work:

```text
Application: /Applications/Hearthstone/Hearthstone.app
Logs:        /Applications/Hearthstone/Logs/Hearthstone_<timestamp>/
log.config:  ~/Library/Preferences/Blizzard/Hearthstone/log.config
```

The client already had every required component enabled, so this project did
not alter the user's `log.config` or restart Hearthstone during validation.

## Legacy behavior

`Sources/logloader.cpp` implements the old bootstrap:

| Legacy function | Behavior | ArenaNext disposition |
| --- | --- | --- |
| `readLogsDirPath` | Defaults to `/Applications/Hearthstone/Logs`, creates missing `Logs`, writes `client.config`. | Discover only; never create Blizzard directories implicitly. |
| `createDefaultLogConfig` | Uses `~/Library/Preferences/Blizzard/Hearthstone/log.config`. | Preserve. |
| `checkLogConfig` | Enables `LoadingScreen`, `Power`, `Zone`, `Arena`, `Asset`; `Power` gets `Verbose=1`. | Preserve settings, redesign merge. |
| `checkLogConfigOption` | Only detects a missing section. | Repair missing or disabled values, preserving unrelated entries. |
| `removeOldLogDirs` | Deletes all but two sessions. | Do not port. |

The exact desired sections are:

```ini
[LoadingScreen]
LogLevel=1
FilePrinting=true

[Power]
LogLevel=1
FilePrinting=true
Verbose=1

[Zone]
LogLevel=1
FilePrinting=true

[Arena]
LogLevel=1
FilePrinting=true

[Asset]
LogLevel=1
FilePrinting=true
```

## ArenaNext implementation

`crates/hs-paths` does read-only discovery. `crates/hs-log-config` reports a
structured `LoggingStatus`; `arena-next --enable-logging` is the only
explicitly mutating command. It creates a timestamped backup and writes the
new file atomically. `arena-next logging inspect|diff|restore` provides the
read-only/reversible workflow. Any change reports
`hearthstoneRestartRequired: true`.

`client.config` size-limit management is intentionally deferred: it is a
separate user-owned config concern and must not be changed silently.
