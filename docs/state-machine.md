# ArenaNext state machine

```text
one native process
  │ checkpoint suffix, or current-deck tail resync
  ▼
typed, source-identified events ──► ArenaReducer ──► native overlay model
                                         │
                                         └─ authoritative deck snapshot replaces prior deck
```

Arena run lifecycle:

```text
unknown → drafting ───────────────────────────────────────→ active deck | rewards
             │
             └─ typed current offer

unknown → redrafting
             │
             ├─ configured normal three-card pick round × pickRounds
             │       └─ explicit confirmed-pick evidence advances this stage
             │
             └─ awaiting discard review
                     └─ explicit capture/manual review adapter
                           → choose `discardCount` cards → fresh deck snapshot

ArenaDeckSnapshotStarted(draftDeckId)
  └─ reading snapshot
       ├─ ArenaDeckSnapshotCard × n
       └─ ArenaDeckSnapshotCompleted → authoritative deck size inferred
```

`REDRAFTING` in the raw log means the Redraft sequence has begun; it does not
by itself identify the later discard-review surface. A selected local rules
manifest provides `pickRounds` and `discardCount`. When the configured rounds
are exhausted, normal three-card capture is stopped conservatively until a
separate review adapter observes that screen.

Manual correction is a separate typed input protocol, never a synthetic log
pick. It may replace a currently visible Draft/eligible Redraft offer or
record an editable discard selection after the proven Redraft boundary. It
cannot add a deck card, infer a missing pick number, or represent the discard
review as another normal offer. See [manual draft correction](manual-draft-correction.md).

Application restart is recovery, not state loss: normal overlay startup first
validates its local append-only observer checkpoint and consumes only a small
newly appended suffix before publishing restored state. If that suffix is too
large, crosses a rotation/order boundary, or the checkpoint is invalid, it
reverse-searches raw `Arena.log` bytes for the newest `Draft Deck ID` snapshot,
parses that compact snapshot plus at most a small Arena suffix, and begins
`LoadingScreen`, `Power`, `Zone`, and `Asset` at EOF. A malformed or incomplete
newest snapshot becomes `awaiting_snapshot`; it never silently replays a large
historical session just to attach.

The snapshot is authoritative for the current deck, but not for old draft
selections or an absolute pick number. Normal Draft can still inspect its
three visible offers with an unknown pick number. Redraft offer capture is
withheld when its five-round progress cannot be proven, so the later discard
screen is never mistaken for a draft offer. Diagnostics expose `attachMethod`
as `verified_checkpoint`, `tail_snapshot`, `tail_run`, `awaiting_snapshot`, or
the explicit-only `full_replay`.

Historical replay is reserved for `arena-next replay` and other commands that
explicitly promise a complete timeline. Rotation, truncation, and session
changes use the same current-deck resync path. Every newly tailed parser line
carries session, component, byte offset and stable line hash, so an identical
line is idempotent. `Client chooses` records are ignored until a complete,
stable sidebar baseline creates a verified boundary; the gate closes again at
every new-run or deck-snapshot boundary.

“Complete replay” means every available record after a proven run boundary.
If a supplied log session begins partway through a run, as a sanitized fixture
can, draft history and absolute pick progress remain `unknown` rather than
being reconstructed from deck size.
