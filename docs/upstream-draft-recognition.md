# Upstream draft-recognition inventory

Reference: `Sources/drafthandler.cpp`, `Sources/drafthandler.h`, and
`Sources/utility.cpp` in `../legacy-arena-tracker`.

## Current conclusion

Live current logs expose the selected card and the reconstructed deck, but no
explicit three-card offer. The legacy app is the same: `GameWatcher` parses
`Client chooses`, while `DraftHandler` identifies offers through screenshots.

## Legacy pipeline

1. Capture every display, not just Hearthstone.
2. Find arena templates with old OpenCV SURF/FLANN/homography.
3. Crop three card-art regions.
4. Compare HSV 50×60 histograms against an eligible-card corpus.
5. Keep 7–15 candidates; accept only after repeated stable matches.
6. Apply hard-coded mana and rarity crop checks.

It samples every 100–200ms and starts with a `0.35` histogram-distance
threshold, relaxing `0.02` per capture. This is a behavioral reference only;
OpenCV 2.4/SURF and full-display capture are not portable forward.

## ArenaNext design

`crates/arena-draft` now has a platform-neutral variable-size crop geometry,
fingerprint matcher, and repeated-frame confidence accumulator. A detection
retains ordered candidates, confidence, normalized dHash distance, source,
and an opt-in in-memory crop reference for each calibrated slot. It withholds
a recommendation unless every expected slot has a sufficiently confident top
candidate and enough stable frames have been observed.

The macOS application captures only the Hearthstone window through
ScreenCaptureKit, normalizes window-relative/Retina geometry, and performs
matching only when an explicit local fingerprint catalog is supplied. It never
falls back to a desktop screenshot. Normal Arena picks and policy-configured
Redraft pick rounds use calibrated three-card geometry; the later Redraft
discard review is a separate state and is not treated as a five-card offer.

This is an implemented local pipeline, not a claim that a public catalog is
currently calibrated for every live client layout. Failed crops remain opt-in
debug material, and a log-originated offer would still be authoritative if a
future client exposes one.
