# Upstream deck-state inventory

Reference: `Sources/gamewatcher.cpp:212`, `Sources/deckhandler.cpp`, and
`Sources/mainwindow.cpp` in `../legacy-arena-tracker`.

## Legacy state machine

```text
noDeckRead → readingDeck → deckRead
```

`OnChoicesAndContents ... Draft Deck ID` starts the read, each `Draft deck
contains card` line adds a card, and `ACTIVE_DRAFT_DECK` completes it.

The old `DeckHandler::newDeckCardAsset(... skipDups=true)` discards duplicate
entries, then tries to repair them from `ArenaTrackerDrafts.json` keyed only by
hero. Current live snapshots prove that is wrong: duplicate card IDs are
meaningful.

## ArenaNext reducer rules

`ArenaDeckSnapshotStarted` clears the previous snapshot and records the real
`draftDeckId`. Each `ArenaDeckSnapshotCard` increments its count—even if the
ID already occurred. `ArenaDeckSnapshotCompleted` makes the snapshot
authoritative.

The public snapshot retains:

```json
{
  "mode": "arena",
  "heroClass": "mage",
  "deck": [{ "cardId": "ETC_536", "count": 2 }],
  "run": { "draftDeckId": "...", "deckSnapshotComplete": true },
  "draft": { "offers": [], "selected": null }
}
```

The deck key is session plus Draft Deck ID, never hero class. Metadata is
joined after reduction, so an unresolved card remains `missing_metadata` and
can never masquerade as a zero-cost card.

