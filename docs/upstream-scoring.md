# Upstream scoring inventory

Reference: `Sources/drafthandler.cpp`, `Sources/winratesdownloader.cpp`,
`Sources/winratesdownloader.h`, and `HearthArena/hearthArena.json` in
`../legacy-arena-tracker`.

| Source | Legacy behavior | ArenaNext decision |
| --- | --- | --- |
| HearthArena | Vendored class/card integer scores. | Local import only until data provenance permits use. |
| HSReplay | Hard-coded undocumented endpoints, day-mtime cache. | Ported: explicit `import-hsreplay`. |
| Firestone | Hard-coded third-party endpoint. | Ported: explicit `import-firestone`. |
| LightForge | File exists but is not used for rating. | Do not port. |

`crates/arena-scoring` provides:

```rust
trait RatingProvider {
    fn metadata(&self) -> &ProviderMetadata;
    fn rating(&self, card_id: &str, class: Option<HeroClass>) -> Option<CardRating>;
    fn provider_ratings(&self, card_id: &str, class: Option<HeroClass>) -> Vec<ProviderRating>;
}
```

`rating` is the joined number a consumer displays by default;
`provider_ratings` keeps every source's own rating (and its own scale) visible
so a single composite never hides the evidence behind it.

## Per-source signals

| Source | Command | Signal | Scale |
| --- | --- | --- | --- |
| HearthArena | `import-heartharena` | parsed public tier-list score | 1-145 tier score |
| HSReplay | `import-hsreplay` | `drawn_win_rate` (`ALL` bucket → generic rows, class buckets → class rows) | win-rate % (0-100) |
| Firestone | `import-firestone [--firestone-format arena\|arena-underground]` | `playedThenWin / played × 100` (unplayed cards omitted) | win-rate % (0-100) |

HSReplay's public card-stats endpoint is Cloudflare-fronted and serves HTTP 403
to non-browser TLS ClientHellos (rustls, Node's OpenSSL) even with a browser
User-Agent on HTTP/1.1. The macOS system curl (SecureTransport) passes, so
`import-hsreplay` shells out to `curl --http1.1 -A <Chrome UA>`; every other
ArenaNext network use stays on `ureq`. Verified 2026-08-05.

Each importer writes its own cache (`heartharena-ratings.json`,
`hsreplay-ratings.json`, `firestone-ratings.json`) in the ArenaNext app-data
directory. `load_live_ratings` joins whichever of those caches exist (or the
single file named by `--ratings`) into a composite. Imports are explicit
commands only. Normal overlay startup never touches the network; the separate
user-triggered deck-row hover preview may fetch and cache one rendered card
from HearthstoneJSON and does not send deck contents or scoring data.

## Composite normalization

The composite joins sources on each card's rating, following the per-source
lookup order (class-specific first, generic fallback second) and averaging
only the sources that have the card. Before joining, every source is mapped
onto a common 0-100 scale with an outlier-aware transform identical to the
official HDT arena helper:

- center = the source's median rating value;
- spread = the source's MAD scaled by `1.4826` (a consistent estimator of the
  standard deviation); a zero spread falls back to a floor so the transform
  stays defined;
- `score = 50 + 50 * tanh((value - center) / spread)`.

A card exactly at a source's median maps to 50; a card a few MADs away
saturates toward 0 or 100 but is never clamped, so outliers remain outliers
instead of being squeezed into a fixed range. The equal-weight mean of
normalized scores is the composite `rating`; `provider_ratings` always exposes
the raw per-source values.

`None` is semantically `No rating`; it never becomes zero. UI work must show
stale data from the timestamp rather than silently treating it as current.
