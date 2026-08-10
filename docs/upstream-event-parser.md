# Upstream event parser inventory

Reference sources: `Sources/logloader.cpp`, `Sources/logworker.cpp`, and
`Sources/gamewatcher.cpp` in `../legacy-arena-tracker`.

## Legacy flow

1. Select the newest subdirectory under `Logs` by modification time.
2. Tail five component files by byte offset.
3. Strip the `D HH:MM:SS.fffffff` timestamp.
4. Sort component lines by synthesized timestamp.
5. Route strings through Qt signals into `GameWatcher`.

Useful legacy behavior is the component set and grammar, not the signal graph.
Its weaknesses are intentional non-replay of historical non-LoadingScreen
logs, no rotation recovery, timestamp collision hacks, and deleted old log
directories.

## ArenaNext parser contract

`crates/hs-log-parser` emits a raw provenance tuple before reducing it:

```text
session + component + byte offset + line number + timestamp
  → RawLogLine
  → GameEvent
  → ArenaReducer
```

`arena-next replay` intentionally replays every available component for
deterministic fixture/debug output. Normal live attach does not: it
reverse-searches the latest authoritative Arena deck snapshot, parses that
small span, and starts gameplay components at EOF. A truncated live file
causes a current-deck resync rather than a historical replay.

Current validated Arena grammar:

```text
DraftManager.OnChoicesAndContents - Draft Deck ID: <id>, Hero Card = HERO_<n>
DraftManager.OnChoicesAndContents - Draft deck contains card <CARD_ID>
SetDraftMode - ACTIVE_DRAFT_DECK
SetDraftMode - DRAFTING|REDRAFTING|IN_REWARDS
Client chooses: <localized card name> (<CARD_ID>)
DraftManager.OnChosen(): hero=HERO_<n>
```

The parser intentionally ignores unneeded board, secret, graveyard, Twitch,
and upload behavior for the first vertical slice.
