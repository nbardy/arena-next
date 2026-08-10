# Local Arena rules manifests

ArenaNext never assumes that an Arena deck contains 30 cards. The log reducer
uses only what it observes unless you explicitly provide a local rules file
with the expected deck size for the current season and mode.

This is an offline input: ArenaNext does not download, update, or infer this
file. That keeps an incomplete reconstruction honest instead of rendering
invented blank cards or an unlabelled `Unknown 5` row.

## Schema version 1

```json
{
  "schemaVersion": 1,
  "season": "your-local-season-label",
  "source": "where this local rule was verified",
  "defaultMode": "the-arena",
  "modes": [
    {
      "id": "the-arena",
      "expectedDeckSlots": 30
    },
    {
      "id": "underground",
      "expectedDeckSlots": 30,
      "redraft": {
        "pickRounds": 5,
        "discardCount": 5
      }
    }
  ]
}
```

`schemaVersion`, `modes`, each `id`, and a nonzero `expectedDeckSlots` are
required. Mode IDs are matched case-insensitively after trimming whitespace.
Use `defaultMode` when a manifest has more than one mode, or select one at the
command line. ArenaNext intentionally refuses an ambiguous multi-mode file;
it does not guess an Arena mode from partial logs.

`redraft` is optional and applies only to modes that explicitly declare it.
When present, it must contain both positive `pickRounds` and `discardCount`.
For example, the local fixture declares a five-round/five-discard Underground
policy. The manifest enables the existing ordinary three-card Redraft-pick
flow; it does not by itself calibrate the separate discard-review surface.

## Use

```bash
cargo run -p arena-next -- replay fixtures/logs/sample-arena-session \
  --cards fixtures/card-data/sample-cards.json \
  --arena-rules fixtures/arena-rules/sample-season.json

cargo run -p arena-next -- doctor \
  --logs fixtures/logs/sample-arena-session \
  --cards fixtures/card-data/sample-cards.json \
  --arena-rules fixtures/arena-rules/sample-season.json

cargo run -p arena-next -- inspect \
  --logs fixtures/logs/sample-arena-session \
  --arena-rules fixtures/arena-rules/sample-season.json \
  --arena-mode underground
```

For the fixture above, the logs contain eight observed cards and the selected
rule states 30 expected slots. The resulting state is therefore:

```json
{
  "expectedSlots": 30,
  "observedSlots": 8,
  "unobservedSlots": 22,
  "completeness": {
    "status": "partial",
    "reason": "unobserved_slots"
  }
}
```

Without `--arena-rules`, the reducer keeps its no-default behavior. A complete
authoritative deck snapshot may establish its own observed size; a partial
snapshot remains explicitly incomplete when no expected-size rule is known.

The selected rule is retained when the observer replays a rotated log or
switches to a newer live session.
