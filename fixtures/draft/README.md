# Draft fixtures

`fingerprint-catalog.schema-fixture.json` is structural only. Its hashes are
synthetic and must never be used for a live recommendation.

When recording a real calibration fixture, capture only the Hearthstone window
with explicit Screen Recording permission, remove account-identifying text,
and pair it with the three expected card IDs and the exact client/versioned
crop geometry. Do not commit whole-desktop screenshots or a bulk card-art
corpus.

For Underground Redraft, record normal three-card pick rounds with the
selected local rules policy (`pickRounds` and `discardCount`) in the fixture
metadata. Do not label the later choose-cards-to-discard review as a five-card
draft offer; it requires its own calibrated/manual adapter.
