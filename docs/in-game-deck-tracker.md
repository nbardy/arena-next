# In-game remaining-deck tracker

ArenaNext keeps two distinct deck representations:

- `ArenaSnapshot::deck` is the authoritative constructed Arena deck.
- `GameState::remaining_deck` is a per-game projection initialized when the
  game starts and consumed by friendly deck-zone transitions.

The original Arena deck is never decremented. This matters for restart,
post-game review, Redraft, and future games in the same run.

## Log boundary

The parser accepts only final `ZoneChangeList.ProcessChanges()` records whose
zone text explicitly crosses the local player's `FRIENDLY DECK` boundary.
Opponent and non-deck transitions are ignored.

Each transition includes the Hearthstone entity ID. The reducer remembers
whether that entity is currently in the deck, making repeated UI/render lines
idempotent.

- `FRIENDLY DECK -> FRIENDLY HAND/PLAY/GRAVEYARD/...` removes one known copy.
- `FRIENDLY HAND/PLAY/... -> FRIENDLY DECK` restores a mulliganed card or adds
  a known generated card shuffled into the deck.
- A second `GameStarted` signal does not reset progress; LoadingScreen and
  Power can both announce the same game.

## Native presentation

Deck rows are sorted by printed mana cost, then name, and show duplicate
counts. During a game the heading reports `remaining / initial`; outside a
game the rows show the constructed Arena deck.

The panel remains click-through. The AppKit host polls the global cursor
position and compares it with deck-row rectangles, so hovering does not
intercept clicks intended for Hearthstone.

On first hover, the app fetches the 256px English rendered card from the
HearthstoneJSON image API on a background thread and caches it at:

```text
~/Library/Application Support/ArenaNext/card-renders/<CARD_ID>.png
```

Later hovers are local. A failed fetch leaves the row and tracker usable and
records the failure in `~/Library/Logs/ArenaNext/app.log`.

