//! A deliberately non-invasive ScreenCaptureKit probe.
//!
//! It never starts capture unless `--capture` is supplied, and it never asks
//! for Screen Recording permission unless `--request-permission` is supplied.
//! The probe does not write frames to disk.

use std::process::ExitCode;

use arena_next_macos_capture::{CaptureOptions, FeatureAvailability, MacosWindowCapture};

fn main() -> ExitCode {
    let capture_requested = std::env::args().any(|argument| argument == "--capture");
    let permission_requested = std::env::args().any(|argument| argument == "--request-permission");
    let provider = MacosWindowCapture::new();
    if let Err(error) = provider.initialize_appkit_runtime() {
        eprintln!("Could not initialize the macOS capture runtime: {error}");
        return ExitCode::FAILURE;
    }
    let capabilities = provider.capabilities();

    println!("ArenaNext macOS direct-window capture probe");
    println!(
        "Screen Recording access: {:?}",
        provider.screen_recording_permission()
    );
    println!(
        "Hearthstone discovery: {:?}",
        capabilities.hearthstone_window_discovery
    );
    println!(
        "Direct window capture: {:?}",
        capabilities.direct_window_capture
    );
    println!("Desktop capture: {:?}", capabilities.full_desktop_capture);

    if permission_requested {
        println!(
            "Permission request result: {:?}",
            provider.request_screen_recording_access()
        );
    }

    if !matches!(
        provider.capabilities().hearthstone_window_discovery,
        FeatureAvailability::Available
    ) {
        eprintln!("Grant Screen Recording permission, then run this probe again.");
        return ExitCode::SUCCESS;
    }

    let windows = match provider.find_hearthstone_windows() {
        Ok(windows) => windows,
        Err(error) => {
            eprintln!("Could not discover Hearthstone: {error}");
            return ExitCode::FAILURE;
        }
    };
    if windows.is_empty() {
        println!("No shareable retail Hearthstone window is open.");
        return ExitCode::SUCCESS;
    }

    for window in &windows {
        println!(
            "Hearthstone window {} · {:?} · {:.0}×{:.0} points",
            window.id, window.title, window.frame.width_points, window.frame.height_points
        );
    }

    if capture_requested {
        match provider.capture_window(&windows[0], CaptureOptions::default()) {
            Ok(frame) => println!(
                "Captured direct window {}: {}×{} BGRA, {} bytes (not written to disk)",
                frame.window_id,
                frame.width_px,
                frame.height_px,
                frame.pixels.len()
            ),
            Err(error) => {
                eprintln!("Could not capture Hearthstone: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        println!("Pass --capture to request one direct Hearthstone-window frame.");
    }

    ExitCode::SUCCESS
}
