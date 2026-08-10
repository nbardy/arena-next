# Upstream card-data inventory

Reference: `Sources/mainwindow.cpp`, `Sources/utility.cpp`,
`Sources/hscarddownloader.cpp`, and `CardsJson/cards.json` in
`../legacy-arena-tracker`.

The legacy app indexes its vendored `cards.json` by ID and consumes `name`,
`cost`, `type`, `classes`, `set`, `collectible`, and `dbfId`. Its missing-card
path coerces absent cost to zero; ArenaNext explicitly rejects that behavior.

## ArenaNext cache

`crates/hs-card-data` has the only permitted resolution states:

```text
resolved
unrevealed
non_card_entity
missing_metadata
invalid_card_id
```

ArenaNext accepts a validated, normalized local cache at:

```text
~/Library/Application Support/ArenaNext/card-data.json
```

The cache carries source, data version, update time, and only the fields needed
by the first overlay. Replacing it does not restart Hearthstone or rebuild Rust
code. The first release intentionally does not ship a network updater; import
tooling must download to a temporary file, validate content/schema, and
atomically promote a new cache only in a later explicit maintenance workflow.
The cache is intentionally outside the repository: card data is Blizzard
content and must not be treated as GPL project source.
