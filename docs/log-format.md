# ArenaNext log format

## Input provenance

Each parsed event preserves enough information to explain or replay it:

```text
session directory
component (LoadingScreen | Power | Zone | Arena | Asset)
byte offset
line number
timestamp key
raw line
stable line hash
```

The timestamp format is currently `D HH:MM:SS.fffffff`. It is ordered only
within a live session; session identity and byte provenance remain the durable
ordering keys.

## Arena events

| Raw arena log form | Typed event | Reducer effect |
| --- | --- | --- |
| `OnBegin - Got new draft deck with ID: <id>` | `ArenaRunStarted` | Resets Arena-owned state for a new run. A newer raw `OnBegin` invalidates an older tail snapshot until the new run emits a complete `Draft Deck ID` snapshot. |
| `Draft Deck ID: <id>, Hero Card = HERO_<n>` | `ArenaDeckSnapshotStarted` | Clears prior snapshot, records run ID and hero. |
| `Draft deck contains card <id>` | `ArenaDeckSnapshotCard` | Increments exact card ID count. |
| `ACTIVE_DRAFT_DECK` | `ArenaDeckSnapshotCompleted` | Marks deck authoritative. |
| `Client chooses: … (<id>)` | gated `ArenaPick` | Ignored until a complete sidebar OCR baseline is checkpointed; afterward valid non-hero choices are picks for that run. New-run/snapshot boundaries close the gate. |
| `SetDraftMode - <mode>` | `ArenaDraftMode` | Updates lifecycle state. |

Draft offers are deliberately not inferred from these events. A calibrated
screen detector may emit a typed `ArenaOffer` with a variable item count,
offer kind, source and confidence. Visual candidates remain presentation-only
until an authoritative deck snapshot confirms the chosen card or package.

In Redraft, the selected local Arena-rules policy separates normal three-card
pick rounds from the later choose-cards-to-discard review. Current logs do not
authoritatively identify that review screen, so it is never parsed as a
five-card offer or inferred from a fifth pick alone.

## Failure mode: stalled component log writers

Observed twice in live sessions (2026-08): when a component reaches
Hearthstone's default 10 MB per-file cap, the client prints `Truncating log,
which has reached the size limit of 10000KB` but the truncation fails and
every component writer stops for the rest of the session while the game keeps
running. `Zone.log` is always the first component to hit the cap. Deck and
draft detection then stop silently.

The 10 MB cap cannot be raised: the macOS client ignores the `FileSize` key in
`log.config` (verified 2026-08-06 — a session launched with `FileSize=200000`
still truncated `Zone.log` at 10000KB and stalled every writer). `hs-log-config`
still writes the cap for correctness on clients that honor it, but it must not
be relied on.

ArenaNext mitigates the stall in order of dependence:

- `hs-observer::rotate_overlarge_component_logs` rewrites any required
  component log that exceeds `ROTATE_AT_BYTES` (9.5 MiB), keeping only its
  newest `ROTATION_KEEP_BYTES` (2 MiB) as a hysteresis budget. Rotation is an
  in-place rewrite, so the game keeps appending; the observer's existing
  copy-truncate resync resumes at the retained tail's last line on the next
  poll. `Arena.log`, which holds the drafted-deck record, is hard-skipped by
  the rotation pass and is never rewritten, so a rotation cannot lose a deck.
  The observer does not replay the retained zone bytes, so an in-progress game
  re-establishes its zone state only from events after the attach point and
  regresses to a conservative awaiting state until its next authoritative
  event; this is a presentation degradation, never a tracker-data loss.
  Rotation runs only when a file is actually overlarge, and per-component
  failures are reported (never silent), so it is a no-op for normal sessions.
- `hs-observer::session_staleness` reports a session whose newest required
  component log has not advanced for `LOG_STALENESS_THRESHOLD` (10 minutes).
  The overlay, `doctor`, and `--once` surface this as "Hearthstone log
  activity stopped …; restart Hearthstone". Attach is read-only, so a restart
  never loses tracker data.
