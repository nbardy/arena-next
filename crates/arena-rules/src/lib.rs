//! Local, versioned Arena rules manifests.
//!
//! This crate deliberately has no network client and does not decide which
//! Hearthstone Arena mode is active. A caller supplies a local JSON manifest
//! and, when necessary, explicitly selects its mode. The resulting expected
//! deck size is passed to the pure Arena reducer as a fact supplied by rules,
//! never as a hidden `30`-card default.

use std::{collections::BTreeSet, fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Increment this when the on-disk JSON contract changes incompatibly.
pub const ARENA_RULES_SCHEMA_VERSION: u32 = 1;

/// A local season/rules file. It may contain multiple Arena modes because the
/// log protocol does not reliably identify every product mode on its own.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArenaRulesManifest {
    pub schema_version: u32,
    #[serde(default)]
    pub season: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub default_mode: Option<String>,
    pub modes: Vec<ArenaModeRules>,
}

/// Rules that apply to one explicitly named Arena mode.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArenaModeRules {
    /// Stable local identifier, such as `the-arena` or `underground`.
    pub id: String,
    /// The expected number of cards in a completed deck for this mode.
    pub expected_deck_slots: u16,
    /// Optional mode-specific Redraft contract. A normal Arena mode omits
    /// this entirely; consumers must not assume all modes offer Redraft.
    #[serde(default)]
    pub redraft: Option<RedraftPolicy>,
}

/// Rules for a mode's Redraft flow. Keeping the two values in one optional
/// object makes an incomplete one-field policy unrepresentable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedraftPolicy {
    /// Number of offer rounds in this Redraft flow.
    pub pick_rounds: u8,
    /// Number of cards the user is expected to replace/discard in total.
    pub discard_count: u8,
}

/// The particular local rule selected for an observer attachment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedArenaRules {
    pub schema_version: u32,
    pub season: Option<String>,
    pub source: Option<String>,
    pub mode_id: String,
    pub expected_deck_slots: u16,
    /// `None` is meaningful: the selected mode has no declared Redraft
    /// contract, so UI/capture code must expose reduced capability rather
    /// than assuming a five-offer flow.
    pub redraft: Option<RedraftPolicy>,
}

impl ArenaRulesManifest {
    /// Read and validate a local manifest. No HTTP, downloads, or cache
    /// updates occur here.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .with_context(|| format!("could not read Arena rules manifest {}", path.display()))?;
        let manifest = serde_json::from_str::<Self>(&contents)
            .with_context(|| format!("could not parse Arena rules manifest {}", path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate the version and mode table before a caller uses a rule.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != ARENA_RULES_SCHEMA_VERSION {
            bail!(
                "unsupported Arena rules schemaVersion {}; expected {}",
                self.schema_version,
                ARENA_RULES_SCHEMA_VERSION
            );
        }
        if self.modes.is_empty() {
            bail!("Arena rules manifest must define at least one mode");
        }

        let mut seen_ids = BTreeSet::new();
        for mode in &self.modes {
            let id = canonical_mode_id(&mode.id);
            if id.is_empty() {
                bail!("Arena rules manifest contains an empty mode id");
            }
            if mode.expected_deck_slots == 0 {
                bail!("Arena rules mode `{}` has expectedDeckSlots 0", mode.id);
            }
            if let Some(redraft) = &mode.redraft {
                if redraft.pick_rounds == 0 || redraft.discard_count == 0 {
                    bail!(
                        "Arena rules mode `{}` redraft pickRounds and discardCount must both be positive",
                        mode.id
                    );
                }
            }
            if !seen_ids.insert(id) {
                bail!(
                    "Arena rules manifest contains duplicate mode id `{}`",
                    mode.id
                );
            }
        }

        if let Some(default_mode) = self.default_mode.as_deref() {
            let default_id = canonical_mode_id(default_mode);
            if default_id.is_empty() {
                bail!("Arena rules manifest has an empty defaultMode");
            }
            if !seen_ids.contains(&default_id) {
                bail!(
                    "Arena rules manifest defaultMode `{default_mode}` does not match a declared mode"
                );
            }
        }
        Ok(())
    }

    /// Select one mode from a validated manifest.
    ///
    /// If the caller does not name a mode, the manifest's `defaultMode` is
    /// used. A single-mode manifest is unambiguous without a default. A
    /// multi-mode manifest without a default intentionally errors rather than
    /// guessing from incomplete log state.
    pub fn resolve(&self, requested_mode: Option<&str>) -> Result<ResolvedArenaRules> {
        self.validate()?;
        let selected = if let Some(requested_mode) = requested_mode {
            let requested_id = canonical_mode_id(requested_mode);
            if requested_id.is_empty() {
                bail!("--arena-mode must not be empty");
            }
            self.modes
                .iter()
                .find(|mode| canonical_mode_id(&mode.id) == requested_id)
                .with_context(|| format!("Arena rules manifest has no mode `{requested_mode}`"))?
        } else if let Some(default_mode) = self.default_mode.as_deref() {
            let default_id = canonical_mode_id(default_mode);
            self.modes
                .iter()
                .find(|mode| canonical_mode_id(&mode.id) == default_id)
                // `validate` already proves this; retain an error instead of
                // panicking if this type is ever constructed programmatically.
                .context("Arena rules manifest defaultMode is invalid")?
        } else if self.modes.len() == 1 {
            &self.modes[0]
        } else {
            let available = self
                .modes
                .iter()
                .map(|mode| mode.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "Arena rules manifest defines multiple modes ({available}); set defaultMode or pass --arena-mode"
            );
        };

        Ok(ResolvedArenaRules {
            schema_version: self.schema_version,
            season: self.season.clone(),
            source: self.source.clone(),
            mode_id: selected.id.clone(),
            expected_deck_slots: selected.expected_deck_slots,
            redraft: selected.redraft.clone(),
        })
    }
}

fn canonical_mode_id(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> ArenaRulesManifest {
        ArenaRulesManifest {
            schema_version: ARENA_RULES_SCHEMA_VERSION,
            season: Some("fixture".to_owned()),
            source: Some("local-test".to_owned()),
            default_mode: Some("the-arena".to_owned()),
            modes: vec![
                ArenaModeRules {
                    id: "the-arena".to_owned(),
                    expected_deck_slots: 30,
                    redraft: None,
                },
                ArenaModeRules {
                    id: "underground".to_owned(),
                    expected_deck_slots: 30,
                    redraft: Some(RedraftPolicy {
                        pick_rounds: 5,
                        discard_count: 5,
                    }),
                },
            ],
        }
    }

    #[test]
    fn resolves_explicit_or_default_mode_without_a_hidden_default_size() {
        let rules = manifest();
        assert_eq!(rules.resolve(None).unwrap().expected_deck_slots, 30);
        assert_eq!(
            rules.resolve(Some("UNDERGROUND")).unwrap().mode_id,
            "underground"
        );
        assert_eq!(
            rules.resolve(Some("underground")).unwrap().redraft,
            Some(RedraftPolicy {
                pick_rounds: 5,
                discard_count: 5,
            })
        );
    }

    #[test]
    fn rejects_ambiguous_or_invalid_manifests() {
        let mut ambiguous = manifest();
        ambiguous.default_mode = None;
        assert!(ambiguous.resolve(None).is_err());

        let mut zero_slots = manifest();
        zero_slots.modes[0].expected_deck_slots = 0;
        assert!(zero_slots.validate().is_err());

        let mut duplicate = manifest();
        duplicate.modes[1].id = "THE-ARENA".to_owned();
        assert!(duplicate.validate().is_err());

        let mut zero_pick_rounds = manifest();
        zero_pick_rounds.modes[1]
            .redraft
            .as_mut()
            .unwrap()
            .pick_rounds = 0;
        assert!(zero_pick_rounds.validate().is_err());

        let mut zero_discards = manifest();
        zero_discards.modes[1]
            .redraft
            .as_mut()
            .unwrap()
            .discard_count = 0;
        assert!(zero_discards.validate().is_err());
    }
}
