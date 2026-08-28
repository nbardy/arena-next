#![deny(unsafe_op_in_unsafe_fn)]

//! Lean, deterministic draft-offer recognition primitives.
//!
//! The platform layer owns window capture and image decoding. This crate only
//! accepts validated raw pixels, crops three window-relative card-art regions,
//! produces a small dHash fingerprint, and compares it with cached card-art
//! fingerprints. It deliberately has no OpenCV, image-decoder, filesystem, or
//! screen-capture dependency.
//!
//! A fingerprint match is evidence, not certainty. The recognizer penalizes a
//! close second match, aggregation requires repeated observations, and callers
//! receive an explicit withheld recommendation whenever the evidence is weak.

use std::{collections::BTreeMap, error::Error as StdError, fmt};

use hs_state::DraftOfferSource;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

mod sidebar;

pub use sidebar::{
    ARENA_DECK_CAPACITY, ARENA_EDITOR_DECK_CAPACITY, DeckCount, ReconciliationKind, SidebarCard,
    SidebarDeckRead, SidebarDeckStatus, SidebarIssue, SidebarReconciliation,
    SidebarTextObservation, interpret_deck_sidebar, reconcile_deck_sidebar,
};

/// The confidence floor used by the default matcher and recommendation gate.
pub const DEFAULT_MIN_CONFIDENCE: f32 = 0.80;

/// dHash compares a 9×8 luminance sample grid to produce 64 comparison bits.
pub const DHASH_SAMPLE_WIDTH: u32 = 9;
pub const DHASH_SAMPLE_HEIGHT: u32 = 8;
pub const DHASH_BITS: u32 = 64;

/// Reject implausibly large capture buffers before copying or cropping them.
/// 128 MiB still accommodates an 8K BGRA window frame.
pub const MAX_RAW_FRAME_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_FRAME_DIMENSION: u32 = 16_384;

/// A window-relative, normalized rectangle. Values must fit entirely in the
/// inclusive `0.0..=1.0` coordinate space.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl NormalizedRect {
    pub fn validate(&self) -> RecognitionResult<()> {
        let values = [self.x, self.y, self.width, self.height];
        if values.iter().any(|value| !value.is_finite()) {
            return Err(RecognitionError::InvalidGeometry(
                "normalized coordinates must be finite",
            ));
        }
        if self.x < 0.0 || self.y < 0.0 || self.width <= 0.0 || self.height <= 0.0 {
            return Err(RecognitionError::InvalidGeometry(
                "normalized rectangle must have a non-negative origin and positive size",
            ));
        }

        // The epsilon only absorbs harmless f32 representation error from
        // values such as `2.0 / 3.0 + 1.0 / 3.0`; the converted pixel rectangle
        // is still clamped to the source frame after validation.
        const EPSILON: f32 = 0.000_001;
        if self.x + self.width > 1.0 + EPSILON || self.y + self.height > 1.0 + EPSILON {
            return Err(RecognitionError::InvalidGeometry(
                "normalized rectangle extends beyond the captured window",
            ));
        }
        Ok(())
    }

    pub fn to_pixel_rect(
        self,
        frame_width: u32,
        frame_height: u32,
    ) -> RecognitionResult<PixelRect> {
        self.validate()?;
        if frame_width == 0 || frame_height == 0 {
            return Err(RecognitionError::EmptyFrame);
        }

        let left = (self.x * frame_width as f32).floor() as u32;
        let top = (self.y * frame_height as f32).floor() as u32;
        let right = ((self.x + self.width) * frame_width as f32)
            .ceil()
            .min(frame_width as f32) as u32;
        let bottom = ((self.y + self.height) * frame_height as f32)
            .ceil()
            .min(frame_height as f32) as u32;

        if right <= left || bottom <= top {
            return Err(RecognitionError::InvalidGeometry(
                "normalized rectangle rounded to an empty pixel crop",
            ));
        }

        Ok(PixelRect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }

    fn intersects(&self, other: &Self) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

/// A concrete crop rectangle after normalized geometry has been resolved
/// against a particular captured window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    pub fn validate_within(self, frame_width: u32, frame_height: u32) -> RecognitionResult<()> {
        if self.width == 0 || self.height == 0 {
            return Err(RecognitionError::EmptyCrop);
        }
        let right = self
            .x
            .checked_add(self.width)
            .ok_or(RecognitionError::CropOutOfBounds)?;
        let bottom = self
            .y
            .checked_add(self.height)
            .ok_or(RecognitionError::CropOutOfBounds)?;
        if right > frame_width || bottom > frame_height {
            return Err(RecognitionError::CropOutOfBounds);
        }
        Ok(())
    }
}

/// Geometry used to crop a variable-sized *offer* from one Hearthstone window
/// capture. Normal Arena drafting currently has three items, while hero and
/// package choices may differ. Redraft's repeated card-pick rounds use the
/// same three-choice offer geometry; its later choose-cards-to-discard review
/// is a separate state, not a variable-sized offer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftCropGeometry {
    #[serde(default = "default_layout_version")]
    pub layout_version: u32,
    pub cards: Vec<NormalizedRect>,
}

/// Window-relative rectangles where the overlay may render the two score
/// lines for each normal three-card Arena offer.
///
/// The rectangles are derived from the card crops instead of maintaining a
/// second independent screen calibration. This keeps recognition and overlay
/// placement aligned when Hearthstone moves the offer in a future layout.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftScoreBadgeGeometry {
    pub layout_version: u32,
    pub badges: [NormalizedRect; 3],
}

/// Reference geometry for the current Arena drafting card areas.
///
/// It is intentionally expressed relative to the Hearthstone *window*, not a
/// desktop. Platform code should use a versioned, fixture-tested calibration
/// when a client UI update changes the Arena layout. It must never fall back to
/// a whole-desktop template search.
const DEFAULT_DRAFT_CARD_ART_RECTS: [NormalizedRect; 3] = [
    NormalizedRect {
        x: 0.169,
        y: 0.235,
        width: 0.152,
        height: 0.326,
    },
    NormalizedRect {
        x: 0.334,
        y: 0.235,
        width: 0.152,
        height: 0.326,
    },
    NormalizedRect {
        x: 0.499,
        y: 0.235,
        width: 0.152,
        height: 0.326,
    },
];

const fn default_layout_version() -> u32 {
    // Version 2 moves the offer into the current left-hand draft stage. Card
    // fingerprints captured with the old full-width layout must not be reused.
    2
}

impl Default for DraftCropGeometry {
    fn default() -> Self {
        Self {
            layout_version: default_layout_version(),
            cards: DEFAULT_DRAFT_CARD_ART_RECTS.to_vec(),
        }
    }
}

impl DraftCropGeometry {
    pub fn validate(&self) -> RecognitionResult<()> {
        if self.layout_version == 0 {
            return Err(RecognitionError::InvalidGeometry(
                "crop layout version must be greater than zero",
            ));
        }
        if self.cards.is_empty() {
            return Err(RecognitionError::InvalidGeometry(
                "offer geometry must contain at least one crop",
            ));
        }
        for card in &self.cards {
            card.validate()?;
        }
        for first in 0..self.cards.len() {
            for second in (first + 1)..self.cards.len() {
                if self.cards[first].intersects(&self.cards[second]) {
                    return Err(RecognitionError::InvalidGeometry(
                        "draft card-art crops must not overlap",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn pixel_rects(
        &self,
        frame_width: u32,
        frame_height: u32,
    ) -> RecognitionResult<Vec<PixelRect>> {
        self.validate()?;
        self.cards
            .iter()
            .copied()
            .map(|card| card.to_pixel_rect(frame_width, frame_height))
            .collect()
    }

    /// Derive three compact score panels immediately below the
    /// corresponding card crops. Normal hero/package layouts are deliberately
    /// rejected because their variable choice counts need separate UI rules.
    pub fn score_badge_geometry(&self) -> RecognitionResult<DraftScoreBadgeGeometry> {
        self.validate()?;
        let cards: &[NormalizedRect; 3] = self.cards.as_slice().try_into().map_err(|_| {
            RecognitionError::InvalidGeometry("score badges require exactly three draft card crops")
        })?;

        // Keep enough horizontal room for labels such as "Deck 82", while
        // preserving a narrow gutter between neighboring offers. Height is
        // relative to the card so the two text rows scale with the game UI.
        const WIDTH_FACTOR: f32 = 1.06;
        const HEIGHT_FACTOR: f32 = 0.24;
        const GAP_FACTOR: f32 = 0.020;

        let badges = cards.map(|card| {
            let width = card.width * WIDTH_FACTOR;
            let height = card.height * HEIGHT_FACTOR;
            let gap = card.height * GAP_FACTOR;
            NormalizedRect {
                x: card.x + (card.width - width) / 2.0,
                y: card.y + card.height + gap,
                width,
                height,
            }
        });
        let geometry = DraftScoreBadgeGeometry {
            layout_version: self.layout_version,
            badges,
        };
        geometry.validate()?;
        Ok(geometry)
    }
}

impl DraftScoreBadgeGeometry {
    pub fn validate(&self) -> RecognitionResult<()> {
        if self.layout_version == 0 {
            return Err(RecognitionError::InvalidGeometry(
                "score badge layout version must be greater than zero",
            ));
        }
        for badge in &self.badges {
            badge.validate()?;
        }
        for first in 0..self.badges.len() {
            for second in (first + 1)..self.badges.len() {
                if self.badges[first].intersects(&self.badges[second]) {
                    return Err(RecognitionError::InvalidGeometry(
                        "draft score badges must not overlap",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn pixel_rects(
        &self,
        frame_width: u32,
        frame_height: u32,
    ) -> RecognitionResult<[PixelRect; 3]> {
        self.validate()?;
        let rects = self
            .badges
            .map(|badge| badge.to_pixel_rect(frame_width, frame_height));
        let [left, middle, right] = rects;
        Ok([left?, middle?, right?])
    }
}

/// Pixel arrangement supplied by the platform capture adapter.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PixelFormat {
    Gray8,
    Rgba8,
    Bgra8,
    Argb8,
}

impl PixelFormat {
    const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Gray8 => 1,
            Self::Rgba8 | Self::Bgra8 | Self::Argb8 => 4,
        }
    }
}

/// A raw capture buffer. `bytes_per_row` permits native capture APIs to retain
/// row padding rather than copying first. The buffer must contain exactly all
/// rows, including that padding.
#[derive(Clone, Debug, PartialEq)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub bytes_per_row: usize,
    pub pixel_format: PixelFormat,
    pub pixels: Vec<u8>,
}

impl RawFrame {
    pub fn validate(&self) -> RecognitionResult<()> {
        validate_dimensions(self.width, self.height)?;
        let minimum_row_bytes = usize::try_from(self.width)
            .ok()
            .and_then(|width| width.checked_mul(self.pixel_format.bytes_per_pixel()))
            .ok_or(RecognitionError::FrameTooLarge)?;
        if self.bytes_per_row < minimum_row_bytes {
            return Err(RecognitionError::InvalidBytesPerRow {
                actual: self.bytes_per_row,
                minimum: minimum_row_bytes,
            });
        }
        let required = self
            .bytes_per_row
            .checked_mul(usize::try_from(self.height).map_err(|_| RecognitionError::FrameTooLarge)?)
            .ok_or(RecognitionError::FrameTooLarge)?;
        if required > MAX_RAW_FRAME_BYTES {
            return Err(RecognitionError::FrameTooLarge);
        }
        if self.pixels.len() != required {
            return Err(RecognitionError::InvalidBufferLength {
                actual: self.pixels.len(),
                expected: required,
            });
        }
        Ok(())
    }

    pub fn to_grayscale(&self) -> RecognitionResult<GrayFrame> {
        self.validate()?;
        let pixel_count = pixel_count(self.width, self.height)?;
        let mut pixels = Vec::with_capacity(pixel_count);
        let bytes_per_pixel = self.pixel_format.bytes_per_pixel();

        for y in 0..usize::try_from(self.height).map_err(|_| RecognitionError::FrameTooLarge)? {
            let row_start = y * self.bytes_per_row;
            for x in 0..usize::try_from(self.width).map_err(|_| RecognitionError::FrameTooLarge)? {
                let offset = row_start + x * bytes_per_pixel;
                let luma = match self.pixel_format {
                    PixelFormat::Gray8 => self.pixels[offset],
                    PixelFormat::Rgba8 => luma(
                        self.pixels[offset],
                        self.pixels[offset + 1],
                        self.pixels[offset + 2],
                    ),
                    PixelFormat::Bgra8 => luma(
                        self.pixels[offset + 2],
                        self.pixels[offset + 1],
                        self.pixels[offset],
                    ),
                    PixelFormat::Argb8 => luma(
                        self.pixels[offset + 1],
                        self.pixels[offset + 2],
                        self.pixels[offset + 3],
                    ),
                };
                pixels.push(luma);
            }
        }

        GrayFrame::new(self.width, self.height, pixels)
    }
}

/// A tightly packed luma frame. It is the only image representation used by
/// hashing and matching after a platform capture has been converted.
#[derive(Clone, Debug, PartialEq)]
pub struct GrayFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl GrayFrame {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> RecognitionResult<Self> {
        validate_dimensions(width, height)?;
        let expected = pixel_count(width, height)?;
        if pixels.len() != expected {
            return Err(RecognitionError::InvalidBufferLength {
                actual: pixels.len(),
                expected,
            });
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    pub fn validate(&self) -> RecognitionResult<()> {
        validate_dimensions(self.width, self.height)?;
        let expected = pixel_count(self.width, self.height)?;
        if self.pixels.len() != expected {
            return Err(RecognitionError::InvalidBufferLength {
                actual: self.pixels.len(),
                expected,
            });
        }
        Ok(())
    }

    pub fn crop(&self, rect: PixelRect) -> RecognitionResult<Self> {
        self.validate()?;
        rect.validate_within(self.width, self.height)?;
        let crop_len = pixel_count(rect.width, rect.height)?;
        let mut pixels = Vec::with_capacity(crop_len);
        let source_width =
            usize::try_from(self.width).map_err(|_| RecognitionError::FrameTooLarge)?;
        let crop_width =
            usize::try_from(rect.width).map_err(|_| RecognitionError::FrameTooLarge)?;
        let start_x = usize::try_from(rect.x).map_err(|_| RecognitionError::FrameTooLarge)?;
        let start_y = usize::try_from(rect.y).map_err(|_| RecognitionError::FrameTooLarge)?;

        for row in 0..usize::try_from(rect.height).map_err(|_| RecognitionError::FrameTooLarge)? {
            let start = (start_y + row) * source_width + start_x;
            pixels.extend_from_slice(&self.pixels[start..start + crop_width]);
        }
        Self::new(rect.width, rect.height, pixels)
    }

    fn pixel(&self, x: u32, y: u32) -> u8 {
        self.pixels[y as usize * self.width as usize + x as usize]
    }
}

/// A compact, versioned dHash value.
///
/// It stays a `u64` in memory but serializes as a fixed-width hexadecimal
/// string. JavaScript cannot exactly represent every 64-bit integer, and the
/// catalog is deliberately language-neutral. Deserialization accepts the old
/// numeric representation as a compatibility bridge for early local fixtures.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DHash(pub u64);

impl Serialize for DHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("{:016x}", self.0))
    }
}

impl<'de> Deserialize<'de> for DHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum WireHash {
            Hex(String),
            LegacyNumber(u64),
        }

        match WireHash::deserialize(deserializer)? {
            WireHash::LegacyNumber(value) => Ok(Self(value)),
            WireHash::Hex(value) => {
                let value = value.trim();
                if value.len() != 16 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(D::Error::custom(
                        "dHash must be a 16-character hexadecimal string",
                    ));
                }
                u64::from_str_radix(value, 16)
                    .map(Self)
                    .map_err(|_| D::Error::custom("dHash hexadecimal value is invalid"))
            }
        }
    }
}

impl DHash {
    pub fn from_grayscale(frame: &GrayFrame) -> RecognitionResult<Self> {
        frame.validate()?;
        let mut hash = 0_u64;
        for y in 0..DHASH_SAMPLE_HEIGHT {
            let sample_y = scaled_coordinate(y, frame.height, DHASH_SAMPLE_HEIGHT);
            for x in 0..(DHASH_SAMPLE_WIDTH - 1) {
                let left = frame.pixel(
                    scaled_coordinate(x, frame.width, DHASH_SAMPLE_WIDTH),
                    sample_y,
                );
                let right = frame.pixel(
                    scaled_coordinate(x + 1, frame.width, DHASH_SAMPLE_WIDTH),
                    sample_y,
                );
                if right > left {
                    let bit = y * (DHASH_SAMPLE_WIDTH - 1) + x;
                    hash |= 1_u64 << bit;
                }
            }
        }
        Ok(Self(hash))
    }

    pub fn hamming_distance(self, other: Self) -> u32 {
        (self.0 ^ other.0).count_ones()
    }
}

/// A cached fingerprint for one normalized card-art crop.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CardArtFingerprint {
    pub card_id: String,
    pub hash: DHash,
}

impl CardArtFingerprint {
    pub fn from_grayscale(
        card_id: impl Into<String>,
        frame: &GrayFrame,
    ) -> RecognitionResult<Self> {
        let card_id = card_id.into();
        if card_id.trim().is_empty() {
            return Err(RecognitionError::InvalidCardId);
        }
        Ok(Self {
            card_id,
            hash: DHash::from_grayscale(frame)?,
        })
    }
}

/// A cache-ready card-art fingerprint collection. The matching algorithm is
/// intentionally small and stable so it can be reproduced by a future native
/// implementation without reviving OpenCV.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FingerprintCatalog {
    pub algorithm: FingerprintAlgorithm,
    pub cards: Vec<CardArtFingerprint>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FingerprintAlgorithm {
    #[default]
    #[serde(rename = "dhash_luma_v1")]
    DHashLumaV1,
}

impl FingerprintCatalog {
    pub fn new(cards: Vec<CardArtFingerprint>) -> RecognitionResult<Self> {
        let catalog = Self {
            algorithm: FingerprintAlgorithm::DHashLumaV1,
            cards,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> RecognitionResult<()> {
        if self.cards.is_empty() {
            return Err(RecognitionError::EmptyFingerprintCatalog);
        }
        let mut card_ids = BTreeMap::new();
        for card in &self.cards {
            if card.card_id.trim().is_empty() {
                return Err(RecognitionError::InvalidCardId);
            }
            if card_ids.insert(card.card_id.as_str(), ()).is_some() {
                return Err(RecognitionError::DuplicateCardId(card.card_id.clone()));
            }
        }
        Ok(())
    }

    /// Ranks cached art by Hamming distance. The top candidate's confidence is
    /// deliberately penalized when the runner-up is too similar; displaying an
    /// uncertain candidate is fine, turning it into a recommendation is not.
    pub fn rank_candidates(
        &self,
        observed: DHash,
        config: MatcherConfig,
    ) -> RecognitionResult<Vec<OfferCandidate>> {
        self.validate()?;
        config.validate()?;

        let mut ranked = self
            .cards
            .iter()
            .map(|card| (card.card_id.clone(), observed.hamming_distance(card.hash)))
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));

        let runner_up_distance = ranked.get(1).map(|(_, distance)| *distance);
        Ok(ranked
            .into_iter()
            .enumerate()
            .take(config.max_candidates)
            .map(|(index, (card_id, distance_bits))| OfferCandidate {
                card_id,
                confidence: if index == 0 {
                    top_candidate_confidence(
                        distance_bits,
                        runner_up_distance,
                        config.minimum_hash_margin_bits,
                    )
                } else {
                    similarity_from_distance(distance_bits)
                },
                distance: Some(distance_bits as f32 / DHASH_BITS as f32),
            })
            .collect())
    }
}

/// Matching policy. `minimum_hash_margin_bits` guards against near-tied card
/// art; it is used to reduce confidence, then `minimum_confidence` controls
/// whether a result may be recommended.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatcherConfig {
    pub minimum_confidence: f32,
    pub minimum_hash_margin_bits: u32,
    pub max_candidates: usize,
}

impl Default for MatcherConfig {
    fn default() -> Self {
        Self {
            minimum_confidence: DEFAULT_MIN_CONFIDENCE,
            minimum_hash_margin_bits: 6,
            max_candidates: 5,
        }
    }
}

impl MatcherConfig {
    pub fn validate(self) -> RecognitionResult<()> {
        if !self.minimum_confidence.is_finite() || !(0.0..=1.0).contains(&self.minimum_confidence) {
            return Err(RecognitionError::InvalidMatcherConfig(
                "minimum confidence must be finite and between zero and one",
            ));
        }
        if self.minimum_hash_margin_bits > DHASH_BITS {
            return Err(RecognitionError::InvalidMatcherConfig(
                "minimum hash margin cannot exceed 64 bits",
            ));
        }
        if self.max_candidates == 0 {
            return Err(RecognitionError::InvalidMatcherConfig(
                "at least one candidate must be retained",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferCandidate {
    pub card_id: String,
    pub confidence: f32,
    /// Normalized dHash Hamming distance (`0.0` is exact, `1.0` is maximally
    /// different). It is absent only for externally supplied candidates.
    pub distance: Option<f32>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedOffer {
    pub candidates: Vec<OfferCandidate>,
    pub source: DraftOfferSource,
    /// An opt-in platform-owned debug reference. The pure core never writes a
    /// capture; it only returns in-memory crops when explicitly requested.
    pub crop_debug_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectionResult {
    /// The exact item count of the calibrated layout used for this frame.
    /// A caller must never treat a partial/incorrect layout as a valid normal
    /// three-card offer.
    pub expected_offer_count: usize,
    pub offers: Vec<DetectedOffer>,
    pub minimum_confidence: f32,
}

impl DetectionResult {
    pub fn is_actionable(&self) -> bool {
        matches!(self.recommendation(), OfferRecommendation::Ready { .. })
    }

    /// Returns only high-confidence top candidates. Callers must render an
    /// "unrecognized offer" state for `None`, never a silent guess.
    pub fn selected_card_ids(&self) -> Option<Vec<String>> {
        match self.recommendation() {
            OfferRecommendation::Ready { card_ids } => Some(card_ids),
            OfferRecommendation::Withheld { .. } => None,
        }
    }

    pub fn recommendation(&self) -> OfferRecommendation {
        if self.expected_offer_count == 0 || self.offers.len() != self.expected_offer_count {
            return OfferRecommendation::Withheld {
                reason: RecommendationWithheldReason::ExpectedOfferCount {
                    expected: self.expected_offer_count,
                    actual: self.offers.len(),
                },
            };
        }
        if !self.minimum_confidence.is_finite() || !(0.0..=1.0).contains(&self.minimum_confidence) {
            return OfferRecommendation::Withheld {
                reason: RecommendationWithheldReason::InvalidConfidenceThreshold,
            };
        }

        let mut card_ids = Vec::with_capacity(self.expected_offer_count);
        for (slot, offer) in self.offers.iter().enumerate() {
            let Some(candidate) = offer.candidates.first() else {
                return OfferRecommendation::Withheld {
                    reason: RecommendationWithheldReason::MissingCandidate { slot },
                };
            };
            if candidate.card_id.trim().is_empty()
                || !candidate.confidence.is_finite()
                || candidate.confidence < self.minimum_confidence
            {
                return OfferRecommendation::Withheld {
                    reason: RecommendationWithheldReason::LowConfidence {
                        slot,
                        confidence: candidate.confidence,
                        required: self.minimum_confidence,
                    },
                };
            }
            card_ids.push(candidate.card_id.clone());
        }
        OfferRecommendation::Ready { card_ids }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum OfferRecommendation {
    Ready {
        card_ids: Vec<String>,
    },
    Withheld {
        reason: RecommendationWithheldReason,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum RecommendationWithheldReason {
    ExpectedOfferCount {
        expected: usize,
        actual: usize,
    },
    MissingCandidate {
        slot: usize,
    },
    LowConfidence {
        slot: usize,
        confidence: f32,
        required: f32,
    },
    InsufficientObservations {
        observed: u32,
        required: u32,
    },
    InvalidConfidenceThreshold,
}

/// In-memory recognition output. Debug crops are returned only when the caller
/// asks for them and must be persisted, if at all, by an explicit opt-in
/// platform debug facility.
#[derive(Clone, Debug, PartialEq)]
pub struct RecognitionOutput {
    pub detection: DetectionResult,
    pub debug_crops: Option<Vec<GrayFrame>>,
}

/// Deterministic recognizer for a platform-owned Hearthstone window frame.
#[derive(Clone, Debug)]
pub struct DraftRecognizer {
    geometry: DraftCropGeometry,
    catalog: FingerprintCatalog,
    config: MatcherConfig,
}

impl DraftRecognizer {
    pub fn new(
        geometry: DraftCropGeometry,
        catalog: FingerprintCatalog,
        config: MatcherConfig,
    ) -> RecognitionResult<Self> {
        geometry.validate()?;
        catalog.validate()?;
        config.validate()?;
        Ok(Self {
            geometry,
            catalog,
            config,
        })
    }

    pub fn detect_frame(&self, frame: &RawFrame) -> RecognitionResult<DetectionResult> {
        Ok(self.analyze_frame(frame, false)?.detection)
    }

    pub fn analyze_frame(
        &self,
        frame: &RawFrame,
        retain_debug_crops: bool,
    ) -> RecognitionResult<RecognitionOutput> {
        self.analyze_grayscale(&frame.to_grayscale()?, retain_debug_crops)
    }

    pub fn analyze_grayscale(
        &self,
        frame: &GrayFrame,
        retain_debug_crops: bool,
    ) -> RecognitionResult<RecognitionOutput> {
        frame.validate()?;
        let rects = self.geometry.pixel_rects(frame.width, frame.height)?;
        let mut crops = Vec::with_capacity(rects.len());
        for rect in rects {
            crops.push(frame.crop(rect)?);
        }
        let mut offers = Vec::with_capacity(crops.len());
        for crop in &crops {
            let candidates = self
                .catalog
                .rank_candidates(DHash::from_grayscale(crop)?, self.config)?;
            offers.push(DetectedOffer {
                candidates,
                source: DraftOfferSource::ScreenCapture,
                crop_debug_ref: None,
            });
        }

        Ok(RecognitionOutput {
            detection: DetectionResult {
                expected_offer_count: self.geometry.cards.len(),
                offers,
                minimum_confidence: self.config.minimum_confidence,
            },
            debug_crops: retain_debug_crops.then_some(crops),
        })
    }
}

/// Multi-frame aggregation policy. Repeated, stable observations are required
/// before a screen-recognition result can turn into a recommendation.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregationConfig {
    pub minimum_frames: u32,
    pub minimum_candidate_margin: f32,
    pub max_candidates: usize,
}

impl Default for AggregationConfig {
    fn default() -> Self {
        Self {
            minimum_frames: 2,
            minimum_candidate_margin: 0.08,
            max_candidates: 5,
        }
    }
}

impl AggregationConfig {
    pub fn validate(self) -> RecognitionResult<()> {
        if self.minimum_frames == 0 {
            return Err(RecognitionError::InvalidAggregationConfig(
                "at least one observation is required",
            ));
        }
        if !self.minimum_candidate_margin.is_finite()
            || !(0.0..=1.0).contains(&self.minimum_candidate_margin)
        {
            return Err(RecognitionError::InvalidAggregationConfig(
                "candidate margin must be finite and between zero and one",
            ));
        }
        if self.max_candidates == 0 {
            return Err(RecognitionError::InvalidAggregationConfig(
                "at least one candidate must be retained",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AggregatedDetection {
    pub detection: DetectionResult,
    pub observed_frames: u32,
    pub minimum_frames: u32,
}

impl AggregatedDetection {
    pub fn is_actionable(&self) -> bool {
        matches!(self.recommendation(), OfferRecommendation::Ready { .. })
    }

    pub fn recommendation(&self) -> OfferRecommendation {
        if self.observed_frames < self.minimum_frames {
            return OfferRecommendation::Withheld {
                reason: RecommendationWithheldReason::InsufficientObservations {
                    observed: self.observed_frames,
                    required: self.minimum_frames,
                },
            };
        }
        self.detection.recommendation()
    }
}

#[derive(Clone, Debug)]
struct CandidateEvidence {
    confidence_sum: f32,
    distance_sum: f32,
    support: u32,
    source: DraftOfferSource,
}

/// Accumulates candidate evidence for one visible offer layout. Callers must
/// create it with the exact calibrated slot count and discard it when the
/// phase/offer changes.
/// Callers should discard this accumulator when the game leaves the draft
/// screen or its offer transition is observed.
#[derive(Clone, Debug)]
pub struct ConfidenceAccumulator {
    config: AggregationConfig,
    minimum_confidence: f32,
    observed_frames: u32,
    expected_offer_count: usize,
    slots: Vec<BTreeMap<String, CandidateEvidence>>,
}

impl ConfidenceAccumulator {
    /// Backwards-compatible normal-draft constructor. New layout-aware code
    /// should call [`Self::for_offer_count`] explicitly.
    pub fn new(config: AggregationConfig, minimum_confidence: f32) -> RecognitionResult<Self> {
        Self::for_offer_count(config, minimum_confidence, 3)
    }

    pub fn for_offer_count(
        config: AggregationConfig,
        minimum_confidence: f32,
        expected_offer_count: usize,
    ) -> RecognitionResult<Self> {
        config.validate()?;
        if !minimum_confidence.is_finite() || !(0.0..=1.0).contains(&minimum_confidence) {
            return Err(RecognitionError::InvalidMatcherConfig(
                "minimum confidence must be finite and between zero and one",
            ));
        }
        if expected_offer_count == 0 {
            return Err(RecognitionError::InvalidGeometry(
                "offer count must be greater than zero",
            ));
        }
        Ok(Self {
            config,
            minimum_confidence,
            observed_frames: 0,
            expected_offer_count,
            slots: (0..expected_offer_count).map(|_| BTreeMap::new()).collect(),
        })
    }

    pub fn observe(&mut self, result: &DetectionResult) -> RecognitionResult<()> {
        if result.expected_offer_count != self.expected_offer_count
            || result.offers.len() != self.expected_offer_count
        {
            return Err(RecognitionError::ExpectedOfferCount {
                expected: self.expected_offer_count,
                actual: result.offers.len(),
            });
        }
        self.observed_frames = self
            .observed_frames
            .checked_add(1)
            .ok_or(RecognitionError::TooManyObservations)?;

        for (slot, offer) in result.offers.iter().enumerate() {
            for candidate in offer.candidates.iter().take(self.config.max_candidates) {
                if candidate.card_id.trim().is_empty() {
                    return Err(RecognitionError::InvalidCardId);
                }
                if !candidate.confidence.is_finite()
                    || candidate.distance.is_some_and(|distance| {
                        !distance.is_finite() || !(0.0..=1.0).contains(&distance)
                    })
                {
                    return Err(RecognitionError::InvalidCandidateEvidence);
                }
                let evidence = self.slots[slot]
                    .entry(candidate.card_id.clone())
                    .or_insert_with(|| CandidateEvidence {
                        confidence_sum: 0.0,
                        distance_sum: 0.0,
                        support: 0,
                        source: offer.source,
                    });
                evidence.confidence_sum += candidate.confidence;
                evidence.distance_sum += candidate.distance.unwrap_or(1.0);
                evidence.support = evidence
                    .support
                    .checked_add(1)
                    .ok_or(RecognitionError::TooManyObservations)?;
            }
        }
        Ok(())
    }

    pub fn finish(&self) -> RecognitionResult<AggregatedDetection> {
        if self.observed_frames == 0 {
            return Err(RecognitionError::NoObservations);
        }

        let mut offers = Vec::with_capacity(self.expected_offer_count);
        for slot in &self.slots {
            let mut candidates = slot
                .iter()
                .map(|(card_id, evidence)| {
                    // Divide by all observed frames, not only supporting frames:
                    // a one-frame appearance cannot masquerade as stable evidence.
                    let confidence = evidence.confidence_sum / self.observed_frames as f32;
                    let distance = evidence.distance_sum / evidence.support as f32;
                    (
                        OfferCandidate {
                            card_id: card_id.clone(),
                            confidence,
                            distance: Some(distance),
                        },
                        evidence.source,
                    )
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|left, right| {
                right
                    .0
                    .confidence
                    .total_cmp(&left.0.confidence)
                    .then_with(|| left.0.card_id.cmp(&right.0.card_id))
            });

            // A stable tie across frames is still ambiguous. Preserve the
            // candidates for UI diagnostics but penalize the top one so the
            // normal confidence gate withholds a recommendation.
            if candidates.len() > 1 && self.config.minimum_candidate_margin > 0.0 {
                let margin = (candidates[0].0.confidence - candidates[1].0.confidence).max(0.0);
                let margin_factor = (margin / self.config.minimum_candidate_margin).min(1.0);
                candidates[0].0.confidence *= margin_factor;
            }

            let source = candidates
                .first()
                .map(|(_, source)| *source)
                .unwrap_or(DraftOfferSource::ScreenCapture);
            candidates.truncate(self.config.max_candidates);
            offers.push(DetectedOffer {
                candidates: candidates
                    .into_iter()
                    .map(|(candidate, _)| candidate)
                    .collect(),
                source,
                crop_debug_ref: None,
            });
        }

        Ok(AggregatedDetection {
            detection: DetectionResult {
                expected_offer_count: self.expected_offer_count,
                offers,
                minimum_confidence: self.minimum_confidence,
            },
            observed_frames: self.observed_frames,
            minimum_frames: self.config.minimum_frames,
        })
    }
}

/// Platform-facing contract retained for adapters that own an opaque capture
/// reference. Such adapters retrieve pixels, call [`DraftRecognizer`], then
/// can return the resulting portable [`DetectionResult`].
pub trait OfferDetector: Send + Sync {
    fn detect(&self, request: DetectionRequest) -> anyhowless::Result<DetectionResult>;
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectionRequest {
    /// A platform-owned opaque reference to a Hearthstone-window-only frame.
    pub capture_ref: String,
    pub window_width: u32,
    pub window_height: u32,
    pub retina_scale: f32,
    pub debug_capture: bool,
}

impl DetectionRequest {
    pub fn validate(&self) -> anyhowless::Result<()> {
        if self.capture_ref.trim().is_empty() || self.window_width == 0 || self.window_height == 0 {
            return Err(anyhowless::Error(
                "capture reference and non-zero window dimensions are required".into(),
            ));
        }
        if !self.retina_scale.is_finite() || self.retina_scale <= 0.0 {
            return Err(anyhowless::Error(
                "retina scale must be finite and greater than zero".into(),
            ));
        }
        Ok(())
    }
}

/// A tiny error wrapper preserves the original public `OfferDetector` contract
/// without adding a general-purpose error dependency to the common interface.
pub mod anyhowless {
    use std::{error::Error as StdError, fmt};

    #[derive(Debug)]
    pub struct Error(pub String);

    impl fmt::Display for Error {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(&self.0)
        }
    }

    impl StdError for Error {}

    pub type Result<T> = std::result::Result<T, Error>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecognitionError {
    EmptyFrame,
    EmptyCrop,
    FrameTooLarge,
    InvalidBytesPerRow { actual: usize, minimum: usize },
    InvalidBufferLength { actual: usize, expected: usize },
    InvalidGeometry(&'static str),
    CropOutOfBounds,
    EmptyFingerprintCatalog,
    DuplicateCardId(String),
    InvalidCardId,
    InvalidMatcherConfig(&'static str),
    InvalidAggregationConfig(&'static str),
    ExpectedOfferCount { expected: usize, actual: usize },
    InvalidCandidateEvidence,
    NoObservations,
    TooManyObservations,
}

impl fmt::Display for RecognitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyFrame => formatter.write_str("frame dimensions must be non-zero"),
            Self::EmptyCrop => formatter.write_str("crop dimensions must be non-zero"),
            Self::FrameTooLarge => formatter.write_str("frame exceeds recognition safety limits"),
            Self::InvalidBytesPerRow { actual, minimum } => write!(
                formatter,
                "frame stride of {actual} bytes is smaller than the required {minimum} bytes"
            ),
            Self::InvalidBufferLength { actual, expected } => write!(
                formatter,
                "frame buffer contains {actual} bytes but {expected} were required"
            ),
            Self::InvalidGeometry(reason) => write!(formatter, "invalid crop geometry: {reason}"),
            Self::CropOutOfBounds => formatter.write_str("crop falls outside the source frame"),
            Self::EmptyFingerprintCatalog => formatter.write_str("fingerprint catalog is empty"),
            Self::DuplicateCardId(card_id) => {
                write!(formatter, "duplicate fingerprint for card {card_id}")
            }
            Self::InvalidCardId => formatter.write_str("card ID must not be empty"),
            Self::InvalidMatcherConfig(reason) => {
                write!(formatter, "invalid matcher config: {reason}")
            }
            Self::InvalidAggregationConfig(reason) => {
                write!(formatter, "invalid aggregation config: {reason}")
            }
            Self::ExpectedOfferCount { expected, actual } => {
                write!(
                    formatter,
                    "expected {expected} offer items, received {actual}"
                )
            }
            Self::InvalidCandidateEvidence => formatter.write_str("candidate evidence is invalid"),
            Self::NoObservations => {
                formatter.write_str("no recognition observations were accumulated")
            }
            Self::TooManyObservations => formatter.write_str("observation counter overflowed"),
        }
    }
}

impl StdError for RecognitionError {}

impl From<RecognitionError> for anyhowless::Error {
    fn from(error: RecognitionError) -> Self {
        Self(error.to_string())
    }
}

pub type RecognitionResult<T> = std::result::Result<T, RecognitionError>;

fn validate_dimensions(width: u32, height: u32) -> RecognitionResult<()> {
    if width == 0 || height == 0 {
        return Err(RecognitionError::EmptyFrame);
    }
    if width > MAX_FRAME_DIMENSION || height > MAX_FRAME_DIMENSION {
        return Err(RecognitionError::FrameTooLarge);
    }
    Ok(())
}

fn pixel_count(width: u32, height: u32) -> RecognitionResult<usize> {
    validate_dimensions(width, height)?;
    let count = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or(RecognitionError::FrameTooLarge)?;
    if count > MAX_RAW_FRAME_BYTES {
        return Err(RecognitionError::FrameTooLarge);
    }
    Ok(count)
}

fn luma(red: u8, green: u8, blue: u8) -> u8 {
    // Integer BT.601 approximation: 0.299 R + 0.587 G + 0.114 B.
    ((77 * red as u16 + 150 * green as u16 + 29 * blue as u16 + 128) >> 8) as u8
}

fn scaled_coordinate(sample: u32, source_len: u32, sample_len: u32) -> u32 {
    ((sample as u64 * source_len as u64) / sample_len as u64) as u32
}

fn similarity_from_distance(distance_bits: u32) -> f32 {
    1.0 - distance_bits as f32 / DHASH_BITS as f32
}

fn top_candidate_confidence(
    distance_bits: u32,
    runner_up_distance: Option<u32>,
    required_margin_bits: u32,
) -> f32 {
    let similarity = similarity_from_distance(distance_bits);
    let Some(runner_up_distance) = runner_up_distance else {
        return similarity;
    };
    if required_margin_bits == 0 {
        return similarity;
    }
    let margin_bits = runner_up_distance.saturating_sub(distance_bits);
    let ambiguity_factor = (margin_bits as f32 / required_margin_bits as f32).min(1.0);
    similarity * ambiguity_factor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray_pattern(kind: u8, width: u32, height: u32) -> GrayFrame {
        let mut pixels = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                let bright = match kind {
                    0 => (x / 3) % 2 == 0,
                    1 => (y / 3) % 2 == 0,
                    _ => ((x / 3) + (y / 3)) % 2 == 0,
                };
                pixels.push(if bright { 224 } else { 24 });
            }
        }
        GrayFrame::new(width, height, pixels).unwrap()
    }

    fn offer_result(card_id: &str, confidence: f32) -> DetectionResult {
        DetectionResult {
            expected_offer_count: 3,
            offers: (0..3)
                .map(|_| DetectedOffer {
                    candidates: vec![OfferCandidate {
                        card_id: card_id.into(),
                        confidence,
                        distance: Some(1.0 - confidence),
                    }],
                    source: DraftOfferSource::ScreenCapture,
                    crop_debug_ref: None,
                })
                .collect(),
            minimum_confidence: DEFAULT_MIN_CONFIDENCE,
        }
    }

    #[test]
    fn raw_frame_validation_rejects_truncated_data_and_handles_padded_bgra() {
        let truncated = RawFrame {
            width: 2,
            height: 1,
            bytes_per_row: 8,
            pixel_format: PixelFormat::Bgra8,
            pixels: vec![0; 7],
        };
        assert!(matches!(
            truncated.validate(),
            Err(RecognitionError::InvalidBufferLength { .. })
        ));

        let padded = RawFrame {
            width: 2,
            height: 1,
            bytes_per_row: 12,
            pixel_format: PixelFormat::Bgra8,
            // blue, green, red, alpha; four bytes of row padding follow.
            pixels: vec![0, 0, 255, 255, 255, 0, 0, 255, 99, 99, 99, 99],
        };
        let luma = padded.to_grayscale().unwrap();
        assert_eq!(luma.pixels, vec![77, 29]);
    }

    #[test]
    fn normalized_geometry_is_ordered_non_overlapping_and_in_bounds() {
        let geometry = DraftCropGeometry::default();
        geometry.validate().unwrap();
        let rects = geometry.pixel_rects(1920, 1080).unwrap();
        let [left, middle, right] = rects.as_slice() else {
            panic!("default geometry must contain exactly three crops");
        };
        assert!(left.x < middle.x && middle.x < right.x);
        assert!(left.width > 0 && left.height > 0);
        right.validate_within(1920, 1080).unwrap();

        let overlapping = DraftCropGeometry {
            layout_version: 1,
            cards: vec![
                NormalizedRect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.5,
                    height: 0.5,
                },
                NormalizedRect {
                    x: 0.4,
                    y: 0.0,
                    width: 0.5,
                    height: 0.5,
                },
                NormalizedRect {
                    x: 0.0,
                    y: 0.5,
                    width: 0.5,
                    height: 0.5,
                },
            ],
        };
        assert!(matches!(
            overlapping.validate(),
            Err(RecognitionError::InvalidGeometry(_))
        ));
    }

    #[test]
    fn current_arena_layout_tracks_the_three_cards_in_the_reference_capture() {
        let geometry = DraftCropGeometry::default();
        let rects = geometry.pixel_rects(3390, 2214).unwrap();
        let centers = rects
            .iter()
            .map(|rect| rect.x + rect.width / 2)
            .collect::<Vec<_>>();

        // Calibrated from the 2026-07 Arena draft fixture. Tolerances allow
        // for the soft/ornamental edges of the Hearthstone card frames.
        assert!(centers[0].abs_diff(831) <= 8);
        assert!(centers[1].abs_diff(1389) <= 8);
        assert!(centers[2].abs_diff(1949) <= 8);
        assert!(rects.iter().all(|rect| rect.y >= 510 && rect.y <= 530));
    }

    #[test]
    fn score_badges_align_with_cards_and_scale_across_frame_sizes() {
        let cards = DraftCropGeometry::default();
        let badges = cards.score_badge_geometry().unwrap();
        badges.validate().unwrap();

        for (card, badge) in cards.cards.iter().zip(badges.badges.iter()) {
            let card_center = card.x + card.width / 2.0;
            let badge_center = badge.x + badge.width / 2.0;
            assert!((card_center - badge_center).abs() < f32::EPSILON);
            assert!(badge.y > card.y + card.height);
        }

        for (width, height) in [(1280, 720), (1920, 1200), (3390, 2214), (3840, 2160)] {
            let rects = badges.pixel_rects(width, height).unwrap();
            assert!(rects[0].x < rects[1].x && rects[1].x < rects[2].x);
            for rect in rects {
                rect.validate_within(width, height).unwrap();
            }
        }
    }

    #[test]
    fn score_badges_reject_nonstandard_choice_counts() {
        let geometry = DraftCropGeometry {
            layout_version: 1,
            cards: DraftCropGeometry::default().cards[..2].to_vec(),
        };
        assert!(matches!(
            geometry.score_badge_geometry(),
            Err(RecognitionError::InvalidGeometry(
                "score badges require exactly three draft card crops"
            ))
        ));
    }

    #[test]
    fn dhash_is_deterministic_and_preserves_monotonic_brightness_changes() {
        let original = gray_pattern(2, 36, 32);
        let adjusted = GrayFrame::new(
            original.width,
            original.height,
            original.pixels.iter().map(|pixel| pixel / 2 + 50).collect(),
        )
        .unwrap();
        assert_eq!(
            DHash::from_grayscale(&original).unwrap(),
            DHash::from_grayscale(&adjusted).unwrap()
        );
    }

    #[test]
    fn dhash_catalog_values_use_hex_without_losing_legacy_numeric_imports() {
        let value = DHash(0x0123_4567_89ab_cdef);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#""0123456789abcdef""#
        );
        assert_eq!(
            serde_json::from_str::<DHash>(r#""0123456789abcdef""#)
                .unwrap()
                .0,
            value.0
        );
        assert_eq!(serde_json::from_str::<DHash>("42").unwrap().0, 42);
    }

    #[test]
    fn fingerprint_catalog_json_has_a_stable_language_neutral_shape() {
        let catalog: FingerprintCatalog = serde_json::from_str(
            r#"{"algorithm":"dhash_luma_v1","cards":[{"cardId":"CS2_029","hash":"0123456789abcdef"}]}"#,
        )
        .unwrap();
        assert_eq!(catalog.algorithm, FingerprintAlgorithm::DHashLumaV1);
        assert_eq!(catalog.cards[0].hash, DHash(0x0123_4567_89ab_cdef));
        assert_eq!(
            serde_json::to_string(&catalog).unwrap(),
            r#"{"algorithm":"dhash_luma_v1","cards":[{"cardId":"CS2_029","hash":"0123456789abcdef"}]}"#
        );
    }

    #[test]
    fn recognizer_matches_three_normalized_window_crops() {
        let cards = [
            gray_pattern(0, 30, 30),
            gray_pattern(1, 30, 30),
            gray_pattern(2, 30, 30),
        ];
        let hashes = [
            DHash::from_grayscale(&cards[0]).unwrap(),
            DHash::from_grayscale(&cards[1]).unwrap(),
            DHash::from_grayscale(&cards[2]).unwrap(),
        ];
        assert!(hashes[0].hamming_distance(hashes[1]) >= 6);
        assert!(hashes[0].hamming_distance(hashes[2]) >= 6);
        assert!(hashes[1].hamming_distance(hashes[2]) >= 6);

        let catalog = FingerprintCatalog::new(vec![
            CardArtFingerprint::from_grayscale("CARD_LEFT", &cards[0]).unwrap(),
            CardArtFingerprint::from_grayscale("CARD_MIDDLE", &cards[1]).unwrap(),
            CardArtFingerprint::from_grayscale("CARD_RIGHT", &cards[2]).unwrap(),
        ])
        .unwrap();
        let geometry = DraftCropGeometry {
            layout_version: 1,
            cards: vec![
                NormalizedRect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.0 / 3.0,
                    height: 1.0,
                },
                NormalizedRect {
                    x: 1.0 / 3.0,
                    y: 0.0,
                    width: 1.0 / 3.0,
                    height: 1.0,
                },
                NormalizedRect {
                    x: 2.0 / 3.0,
                    y: 0.0,
                    width: 1.0 / 3.0,
                    height: 1.0,
                },
            ],
        };
        let recognizer = DraftRecognizer::new(geometry, catalog, MatcherConfig::default()).unwrap();

        let mut pixels = Vec::with_capacity(90 * 30);
        for y in 0..30 {
            for card in &cards {
                let start = y * 30;
                pixels.extend_from_slice(&card.pixels[start..start + 30]);
            }
        }
        let raw = RawFrame {
            width: 90,
            height: 30,
            bytes_per_row: 90,
            pixel_format: PixelFormat::Gray8,
            pixels,
        };
        let output = recognizer.analyze_frame(&raw, true).unwrap();
        assert_eq!(output.debug_crops.as_ref().map(Vec::len), Some(3));
        assert_eq!(
            output.detection.selected_card_ids(),
            Some(vec![
                "CARD_LEFT".into(),
                "CARD_MIDDLE".into(),
                "CARD_RIGHT".into(),
            ])
        );
    }

    #[test]
    fn close_hash_tie_is_withheld_instead_of_recommended() {
        let catalog = FingerprintCatalog::new(vec![
            CardArtFingerprint {
                card_id: "A".into(),
                hash: DHash(0),
            },
            CardArtFingerprint {
                card_id: "B".into(),
                hash: DHash(1),
            },
        ])
        .unwrap();
        let candidates = catalog
            .rank_candidates(DHash(0), MatcherConfig::default())
            .unwrap();
        assert!(candidates[0].confidence < DEFAULT_MIN_CONFIDENCE);
        let result = DetectionResult {
            expected_offer_count: 3,
            offers: (0..3)
                .map(|_| DetectedOffer {
                    candidates: candidates.clone(),
                    source: DraftOfferSource::ScreenCapture,
                    crop_debug_ref: None,
                })
                .collect(),
            minimum_confidence: DEFAULT_MIN_CONFIDENCE,
        };
        assert!(matches!(
            result.recommendation(),
            OfferRecommendation::Withheld {
                reason: RecommendationWithheldReason::LowConfidence { .. }
            }
        ));
        assert_eq!(result.selected_card_ids(), None);
    }

    #[test]
    fn aggregation_requires_repeated_stable_observations() {
        let mut accumulator =
            ConfidenceAccumulator::new(AggregationConfig::default(), DEFAULT_MIN_CONFIDENCE)
                .unwrap();
        accumulator.observe(&offer_result("CS2_029", 0.95)).unwrap();
        let first = accumulator.finish().unwrap();
        assert!(matches!(
            first.recommendation(),
            OfferRecommendation::Withheld {
                reason: RecommendationWithheldReason::InsufficientObservations { .. }
            }
        ));

        accumulator.observe(&offer_result("CS2_029", 0.95)).unwrap();
        let second = accumulator.finish().unwrap();
        assert!(second.is_actionable());
        assert_eq!(
            second.recommendation(),
            OfferRecommendation::Ready {
                card_ids: vec!["CS2_029".into(), "CS2_029".into(), "CS2_029".into()],
            }
        );
    }

    #[test]
    fn calibrated_five_item_layout_is_not_forced_through_three_card_logic() {
        let geometry = DraftCropGeometry {
            layout_version: 7,
            cards: (0..5)
                .map(|slot| NormalizedRect {
                    x: slot as f32 * 0.2,
                    y: 0.1,
                    width: 0.18,
                    height: 0.5,
                })
                .collect(),
        };
        assert_eq!(geometry.pixel_rects(1000, 500).unwrap().len(), 5);

        let result = DetectionResult {
            expected_offer_count: 5,
            offers: (0..5)
                .map(|_| DetectedOffer {
                    candidates: vec![OfferCandidate {
                        card_id: "CS2_029".into(),
                        confidence: 0.95,
                        distance: Some(0.05),
                    }],
                    source: DraftOfferSource::ScreenCapture,
                    crop_debug_ref: None,
                })
                .collect(),
            minimum_confidence: DEFAULT_MIN_CONFIDENCE,
        };
        let mut accumulator = ConfidenceAccumulator::for_offer_count(
            AggregationConfig::default(),
            DEFAULT_MIN_CONFIDENCE,
            5,
        )
        .unwrap();
        accumulator.observe(&result).unwrap();
        accumulator.observe(&result).unwrap();

        assert_eq!(
            accumulator.finish().unwrap().recommendation(),
            OfferRecommendation::Ready {
                card_ids: vec![
                    "CS2_029".into(),
                    "CS2_029".into(),
                    "CS2_029".into(),
                    "CS2_029".into(),
                    "CS2_029".into(),
                ],
            }
        );
    }

    #[test]
    fn low_confidence_detection_cannot_become_a_recommendation() {
        let result = offer_result("CS2_029", 0.79);
        assert!(!result.is_actionable());
        assert_eq!(result.selected_card_ids(), None);
    }
}
