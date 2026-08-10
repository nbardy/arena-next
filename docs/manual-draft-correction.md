# Manual draft correction protocol

Screen recognition is evidence, not game truth. The native UI must keep
manual correction available while recognition is running and route every
interaction through `ArenaReducer::apply_manual_action`. It must not invent a
authoritative deck snapshot or edit deck counts.

The shared `ManualDraftAction` protocol is serializable and deliberately
small:

```json
{
  "action": "replace_offer",
  "kind": "cards",
  "items": [
    { "kind": "card", "card_id": "EX1_277" },
    { "kind": "unknown", "label": "middle card unreadable" },
    { "kind": "card", "card_id": "CS2_029" }
  ]
}
```

Use it for a click on a detected candidate, a search result, a pasted stable
card ID, or marking an individual slot unknown. A successful correction
replaces only `draft.current_offer`, marks its source as `manual`, and derives
the visible pick number from reducer state. It does **not** select a card,
advance the draft, add a card to the deck, or claim a missing absolute pick
number after a tail resync. Hearthstone's later authoritative deck snapshot remains
the authoritative selection.

Manual offers are allowed only in a known `DRAFTING` or eligible normal
`REDRAFTING` pick round. The reducer rejects a normal offer while Redraft is
awaiting/reviewing discards, so a five-card deck review can never be turned
into a sixth three-card pick by the UI.

## Redraft discard review

The separate review can be driven without calibrated capture using these
actions:

```json
{ "action": "begin_redraft_discard_review" }
{ "action": "set_redraft_discard_selections", "card_ids": ["A", "B", "C", "D", "E"] }
{ "action": "complete_redraft_discard_review" }
```

They are accepted only after the selected local Arena-rules policy has proven
that all normal Redraft pick rounds completed. The editable list may have
fewer than the required number while the player changes it, but never more;
completion requires exactly `discardCount`. Duplicates are valid because a
deck may contain multiple copies. The selection is provisional UI evidence:
it never decrements the local deck. A later complete Arena deck snapshot is
the only authority for discarded cards.

## UI and diagnostics contract

The first native input implementation should expose all of the following at
all times that a draft offer or review is visible:

- replace a detected offer slot from its candidates;
- search a locally normalized card index and select an ID;
- paste a card ID;
- mark a slot unknown;
- replace or clear the pending Redraft discard list before submission;
- save a local, opt-in incorrect-match report.

An incorrect-match report must be local and sanitized: include action/result,
recognition candidates/confidences, crop layout/version, and bounded parser
context; do not upload it automatically or include a whole-desktop capture.
The protocol deliberately does not implement a network report endpoint.

`ManualDraftActionError` is user-displayable. In particular, surface a
withheld Redraft offer as a state explanation rather than silently accepting
it. The app has no live-capture fixture for the current discard-review layout
yet, so a calibrated automatic review detector remains out of scope.
