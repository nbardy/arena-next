//! Platform-neutral status popup contract.
//!
//! This crate intentionally contains no windowing, text, image, or rendering
//! dependencies. Native hosts (AppKit, Win32, and future Linux hosts) turn the
//! model into their local panel/popover, while sharing the same labels,
//! commands, and geometry.

use serde::{Deserialize, Serialize};

pub const POPUP_WIDTH: f32 = 320.0;
pub const POPUP_HEIGHT: f32 = 220.0;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HearthstoneStatus {
    Running,
    #[default]
    NotDetected,
    PermissionRequired,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArenaPhase {
    #[default]
    Unknown,
    Draft,
    Redraft,
    ActiveDeck,
    Rewards,
}

impl ArenaPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Draft => "Draft",
            Self::Redraft => "Redraft",
            Self::ActiveDeck => "Active deck",
            Self::Rewards => "Rewards",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub current: u8,
    pub total: u8,
}

impl Progress {
    pub fn label(&self) -> String {
        if self.total == 0 || self.current == 0 {
            "Unknown".to_owned()
        } else {
            format!("{} / {}", self.current, self.total)
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunStats {
    pub wins: u16,
    pub losses: u16,
}

impl RunStats {
    pub fn label(&self) -> String {
        format!("{}–{}", self.wins, self.losses)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckSummary {
    pub hero: Option<String>,
    pub observed: u16,
    pub expected: Option<u16>,
}

impl DeckSummary {
    pub fn label(&self) -> String {
        let hero = self.hero.as_deref().unwrap_or("Unknown hero");
        match self.expected {
            Some(expected) => format!("{hero} · {}/{} observed", self.observed, expected),
            None => format!("{hero} · {} observed", self.observed),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PopupCommand {
    ShowOverlay,
    HideOverlay,
    ToggleInteraction,
    OpenSettings,
    Quit,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusPopupModel {
    pub hearthstone: HearthstoneStatus,
    pub deck: DeckSummary,
    pub arena_phase: ArenaPhase,
    pub pick_progress: Option<Progress>,
    pub run: RunStats,
    /// A 0–0 score can be either a fresh active run or no run at all; keep
    /// that distinction explicit for the status card.
    #[serde(default)]
    pub active_run: bool,
    pub overlay_visible: bool,
    pub interaction_enabled: bool,
}

impl StatusPopupModel {
    /// Compact text fallback used by native hosts that do not yet have a
    /// richer painter. The same model remains suitable for a fully styled
    /// popup on macOS and Windows.
    pub fn display_text(&self) -> String {
        let mut lines = vec!["HearthAI".to_owned(), String::new()];
        lines.extend(
            self.rows()
                .into_iter()
                .map(|row| format!("{}  {}", row.label, row.value)),
        );
        lines.join("\n")
    }

    pub fn hearthstone_label(&self) -> &'static str {
        match self.hearthstone {
            HearthstoneStatus::Running => "Running",
            HearthstoneStatus::NotDetected => "Not detected",
            HearthstoneStatus::PermissionRequired => "Permission required",
        }
    }

    pub fn pick_label(&self) -> String {
        self.pick_progress
            .as_ref()
            .map_or_else(|| "Unknown".to_owned(), Progress::label)
    }

    pub fn run_label(&self) -> String {
        if self.active_run {
            self.run.label()
        } else {
            "Not active".to_owned()
        }
    }

    pub fn rows(&self) -> [PopupRow; 3] {
        [
            PopupRow::new("Active deck", self.deck.label()),
            PopupRow::new("Mode", self.arena_phase.label()),
            PopupRow::new("Pick", self.pick_label()),
        ]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PopupRow {
    pub label: &'static str,
    pub value: String,
}

impl PopupRow {
    fn new(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: value.into(),
        }
    }
}

/// Logical-point rectangles shared by native renderers. Coordinates are
/// relative to the popup origin and can be scaled by a platform backing scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PopupLayout {
    pub bounds: Rect,
    pub icon: Rect,
    pub rows: [Rect; 3],
    pub overlay_button: Rect,
    pub settings_button: Rect,
    pub quit_button: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

pub fn layout() -> PopupLayout {
    PopupLayout {
        bounds: Rect {
            x: 0.0,
            y: 0.0,
            width: POPUP_WIDTH,
            height: POPUP_HEIGHT,
        },
        icon: Rect {
            x: 16.0,
            y: 16.0,
            width: 48.0,
            height: 48.0,
        },
        rows: [
            Rect {
                x: 78.0,
                y: 100.0,
                width: 226.0,
                height: 22.0,
            },
            Rect {
                x: 78.0,
                y: 124.0,
                width: 226.0,
                height: 22.0,
            },
            Rect {
                x: 78.0,
                y: 148.0,
                width: 226.0,
                height: 22.0,
            },
        ],
        overlay_button: Rect {
            x: 16.0,
            y: 194.0,
            width: 116.0,
            height: 20.0,
        },
        settings_button: Rect {
            x: 140.0,
            y: 194.0,
            width: 82.0,
            height: 20.0,
        },
        quit_button: Rect {
            x: 230.0,
            y: 194.0,
            width: 74.0,
            height: 20.0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_rows_are_truthful_and_cross_platform() {
        let model = StatusPopupModel {
            hearthstone: HearthstoneStatus::Running,
            deck: DeckSummary {
                hero: Some("Mage".into()),
                observed: 25,
                expected: Some(30),
            },
            arena_phase: ArenaPhase::Redraft,
            pick_progress: Some(Progress {
                current: 3,
                total: 5,
            }),
            run: RunStats { wins: 4, losses: 1 },
            ..Default::default()
        };
        let rows = model.rows();
        assert_eq!(rows[0].value, "Mage · 25/30 observed");
        assert_eq!(rows[1].value, "Redraft");
        assert_eq!(rows[2].value, "3 / 5");
        assert_eq!(layout().bounds.width, POPUP_WIDTH);
    }

    #[test]
    fn unknown_progress_never_looks_like_zero() {
        assert_eq!(Progress::default().label(), "Unknown");
        assert_eq!(StatusPopupModel::default().pick_label(), "Unknown");
        assert_eq!(StatusPopupModel::default().run_label(), "Not active");
    }

    #[test]
    fn controls_fit_inside_popup() {
        let l = layout();
        for rect in [l.overlay_button, l.settings_button, l.quit_button] {
            assert!(rect.x >= 0.0 && rect.y >= 0.0);
            assert!(rect.x + rect.width <= l.bounds.width);
            assert!(rect.y + rect.height <= l.bounds.height);
        }
    }
}
