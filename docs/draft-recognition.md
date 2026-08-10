# Draft recognition without a heavyweight runtime

ArenaNext does not ship card-art images, OpenCV, a browser, or a background
download service. The macOS app can opt into draft matching only when it is
started with a local fingerprint catalog:

```bash
HearthAI.app/Contents/MacOS/arena-next \
  --draft-fingerprints /path/to/draft-fingerprints.json \
  --ratings /path/to/local-ratings.json
```

The overlay captures only the current retail Hearthstone window through
ScreenCaptureKit. It never falls back to a desktop screenshot. Frames stay in
memory, are converted to a small dHash, and are discarded after matching.

## Catalog format

The current v0.1 matcher accepts this compact JSON shape:

```json
{
  "algorithm": "dhash_luma_v1",
  "cards": [
    { "cardId": "CS2_029", "hash": "0123456789abcdef" }
  ]
}
```

`hash` is always a 16-character hexadecimal string. The application accepts a
legacy numeric hash only for local early-development fixtures; new catalogs
must use hexadecimal so any future JavaScript or JSON consumer keeps all
64 bits exactly.

Keep the catalog outside the `.app`. A complete hash catalog is small compared
with card art, and can be refreshed independently as the Arena pool changes.
The recommended generation input is square raw card art rather than a full
rendered card frame; generation belongs in a release/development pipeline,
not in the overlay process.

## Safety and confidence gate

The app captures only while logs show an unresolved ordinary three-card Arena
pick: a normal draft pick or a configured Redraft pick round.
It attempts a direct-window frame at most once every 800 ms and clears all
accumulated evidence after a pick or draft transition.

Two stable frames are required before an offer can be recommended. Until then,
the overlay labels output as a candidate and explicitly says why recognition
is withheld. Missing local ratings render as `No rating`; shown ratings include
the provider, timestamp, Arena season/version when supplied, sample size, and
a stale warning after 60 days.

## Calibration before enabling a public catalog

The crop geometry must be validated against sanitized direct-window captures
from the current Hearthstone client, including native fullscreen and Retina
layouts. Do not publish a catalog or claim live recommendations merely because
the hash pipeline compiles. Save fixture metadata and expected candidates, not
unconsented desktop screenshots or card-art bulk downloads, in the source
repository.

## Redraft and non-three-item offers

The local Underground fixture models Redraft as two distinct steps:

1. A configured number of ordinary three-card pick rounds (five in the local
   Underground fixture).
2. A separate deck-review step in which the player chooses cards to discard
   (five in that fixture).

The normal direct-window three-card crop is used only for the first step and
only while the selected local Arena-rules policy says pick rounds remain.
After the final configured Redraft pick, ArenaNext stops normal crop matching.
It does not model the discard review as a five-card offer, and it does not
claim logs have identified that screen merely because the pick counter ended.

The shared offer geometry remains variable-sized for genuine hero/package or
future offer layouts. Redraft review needs its own calibrated capture/manual
adapter and distinct selection state before ArenaNext can identify cards to
discard; no review geometry is invented here.

Recognition must never lock out correction. While an offer is visible, the
native UI can replace the candidates with a stable card ID or an explicit
unknown slot through the shared manual-correction protocol. In Redraft, that
protocol refuses normal-offer corrections after the configured five pick
rounds and instead exposes the distinct editable discard-review actions. See
[manual draft correction](manual-draft-correction.md).
