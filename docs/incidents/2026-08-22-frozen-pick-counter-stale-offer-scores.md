# Frozen pick counter withheld later offer scores

## Symptom

During an Arena draft, the deck tracker continued to show the correct sidebar
count while the three score badges disappeared on later offers. The affected
offer was `Cyborg Patriarch`, `Annihilation`, and `Soul Seeker` at 22/30 cards.

## Root cause

Hearthstone's log-derived `phasePickCount` stopped advancing at 18 even though
sidebar OCR observed 22 cards. `OfferOcrWorker` treated a successfully confirmed
offer as permanent until the log-derived draft key changed. Because that key
never advanced, it stopped capturing and kept the prior offer internally; the
main overlay therefore had no scores for later visible cards.

This was independent of the macOS focus rule that hides all overlay windows
when Hearthstone is not frontmost.

## Fix

ArenaNext 0.1.8:

- rechecks a confirmed offer every 1.5 seconds;
- requires two matching OCR frames before replacing the displayed scores;
- preserves confirmed scores through transient capture/OCR failures;
- invalidates the offer key immediately when the observed deck-slot count
  changes; and
- records OCR retry and confirmation results in the app log.

## Verification

- `cargo test --workspace` passed.
- The packaged and installed 0.1.8 executables matched by SHA-256.
- Live OCR repeatedly read the current three titles and logged the resolved IDs
  `TIME_046`, `JAIL_510`, and `MAW_004`.
- AppKit reported the three 276x84 score panels onscreen above the current
  Hearthstone draft cards.

The installed development build remains ad-hoc signed. Rebuilding changes its
code-directory hash, so launch it from the already-authorized terminal during
development or grant the replacement app Screen Recording access again.
