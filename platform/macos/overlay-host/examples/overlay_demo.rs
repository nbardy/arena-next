//! Run with:
//! `cargo run --manifest-path platform/macos/overlay-host/Cargo.toml --example overlay_demo`
//!
//! The demo performs a small native tick every 500 ms. A production observer
//! should do blocking log work on its own worker, then expose a cheap snapshot
//! for this main-thread callback to render.

use std::time::Duration;

use arena_next_macos_overlay::{
    OverlayBounds, OverlayError, OverlayHost, OverlayModel, TickControl, hearthstone_is_frontmost,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let overlay = OverlayHost::new(
        OverlayBounds::new(80.0, 80.0, 300.0, 130.0),
        &OverlayModel {
            title: "ArenaNext".to_owned(),
            lines: vec![
                "Native macOS overlay foundation".to_owned(),
                "Click-through • all Spaces • fullscreen auxiliary".to_owned(),
            ],
            deck_rows: Vec::new(),
        },
    )?;
    let mut ticks = 0_u64;
    overlay.run_with_tick(Duration::from_millis(500), |host| {
        ticks += 1;
        let game_is_active = hearthstone_is_frontmost()?;
        if game_is_active {
            host.show()?;
        } else {
            host.hide()?;
        }
        host.update_model(&OverlayModel {
            title: "ArenaNext".to_owned(),
            lines: vec![
                if game_is_active {
                    "Hearthstone is active".to_owned()
                } else {
                    "Hearthstone is inactive; overlay hidden".to_owned()
                },
                format!("Native observer tick {ticks}"),
            ],
            deck_rows: Vec::new(),
        })?;
        Ok::<TickControl, OverlayError>(TickControl::Continue)
    })?;
    Ok(())
}
