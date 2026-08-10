# Authoritative tail-resync contract

A live cold attach may hydrate the current Arena deck from a verified,
complete deck snapshot without replaying the entire run. It reverse-searches
raw `Arena.log` bytes for the newest snapshot marker, then parses only that
compact snapshot and a bounded Arena suffix. It does not parse historical
`Power.log`, `Zone.log`, or `Asset.log` records. This is a deck-state
resynchronization, not a claim that prior drafting history was reconstructed.

The observer must emit exactly one synthetic core event after it has proved a
complete snapshot. It includes a current mode only when that mode was observed
at or after the snapshot boundary:

```rust
GameEvent::ArenaAuthoritativeResync {
    draft_deck_id,
    hero_card_id,
    card_ids,
    draft_mode,
}
```

The reducer then exposes:

```text
run.state_origin = authoritative_resync
draft.history_status = partial(authoritative_resync)
draft.phase_progress_status = unknown
```

`card_ids` is authoritative for the deck at the snapshot boundary, including
duplicates. It is not provenance for historical individual picks. The event
clears old offers and selections and starts `pickNumber` at `0` (unknown), not
at one.

## Required proof before emitting the event

- Locate the newest raw `Draft Deck ID` marker by reverse byte search. If that
  newest marker is incomplete, do not reuse an older deck.
- If a later raw Arena `OnBegin` exists, it may start a newer run; wait for
  that run's snapshot instead of reusing the old deck.
- Parse that one snapshot through its completion marker, then parse at most
  the configured compact Arena suffix.
- Preserve duplicate card IDs exactly.
- Take `draft_mode` only from a mode record known to be current at or after
  the snapshot boundary. If unavailable, send `None`; never borrow an older
  mode.
- A malformed or truncated newest snapshot produces `awaiting_snapshot` and
  tails new records; it does not silently trigger a history replay.
- Do not manufacture per-card provenance from the synthetic aggregate event.

Complete historical replay remains available only through an explicit command
that needs it (`arena-next replay`, or `explain-card`, which reconstructs
history to report source provenance). Normal overlay startup, `doctor`,
`inspect`, `--once`, and analysis use current-state attachment.

## Checkpoint restore

A valid local checkpoint may catch up only a small newly appended suffix before
it is exposed. If that suffix exceeds the live budget, encounters a rotation,
or violates cross-component ordering, the observer discards the checkpoint
state and follows this tail-resync contract instead. In particular, an
`OnBegin` appended after a checkpoint is reduced before restored state is
published, so an old run's deck is never briefly presented as current.

## Attach diagnostics

`inspect --json` and `doctor --json` expose `attachMethod` and
`attachDiagnostics`. The latter records the raw snapshot byte offset and parse
size, whether an Arena suffix was skipped because it exceeded the live budget,
whether a later `OnBegin` invalidated a candidate snapshot, how many checkpoint
suffix bytes were consumed before restore, and whether non-Arena components
started at their final complete-line cursor. This makes a slow or incomplete
attach visible without turning on verbose log parsing.

## Draft and Redraft behavior after resync

Current clients emit `Client chooses` for unconfirmed hero and Legendary Group
preview clicks. Those records remain disabled until two equal, complete
`Your Deck` OCR reads establish an authoritative baseline. Only later valid
non-hero choices extend that run. A partial/scrolled sidebar never replaces
hidden cards or opens the choice gate.

For `REDRAFTING`, `redraft.pickProgressKnown` is false after a resync even
when the selected rules manifest declares concrete pick-round and discard
counts. Do not infer the round from a 30- or 35-card count. Normal three-card
screen capture remains withheld until a trustworthy Redraft phase boundary/full
history establishes progress. The later discard review remains a separate
state, never a five-card offer.
