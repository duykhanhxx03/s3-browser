//! s3browser — a desktop S3 client built on GPUI.
//!
//! Runs on macOS, Windows and Linux; everything that differs between them is
//! isolated in [`platform`].

// Hide the console window that Windows would otherwise open alongside the GUI.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod browser;
#[cfg(target_os = "macos")]
mod glass_check;
mod platform;
mod theme;

use std::time::Duration;

use gpui::{px, size, App, AppContext as _, Application, Bounds, WindowOptions};

use browser::Browser;
use platform::Chrome;

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_tokio::init(cx);

        let chrome = Chrome::detect();
        let bounds = Bounds::centered(None, size(px(1120.), px(720.)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: platform::titlebar_options(),
                window_background: chrome.window_background(),
                window_min_size: Some(size(px(720.), px(420.))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Browser::new(window, cx)),
        )
        .expect("failed to open window");

        cx.activate(true);

        if std::env::args().any(|arg| arg == "--verify-glass") {
            schedule_glass_check(cx);
        }
    });
}

/// Waits for the first frames to land, prints what the window manager actually
/// did, then exits with a status CI can gate on.
#[cfg(target_os = "macos")]
fn schedule_glass_check(cx: &mut App) {
    let executor = cx.background_executor().clone();
    cx.spawn(async move |cx| {
        executor.timer(Duration::from_millis(1200)).await;
        _ = cx.update(|cx| {
            let mtm = objc2_foundation::MainThreadMarker::new()
                .expect("glass check must run on the main thread");
            let report = glass_check::inspect(mtm);
            println!("--- glass check ---");
            for line in &report.lines {
                println!("{line}");
            }
            println!(
                "--- {} ---",
                if report.passed {
                    "glass OK"
                } else {
                    "glass FAILED"
                }
            );
            cx.quit();
            if !report.passed {
                std::process::exit(1);
            }
        });
    })
    .detach();
}

/// The check inspects AppKit directly, so there is nothing to verify elsewhere;
/// on Windows blur comes from Acrylic and on Linux only KWin offers it, which
/// [`Chrome::detect`] already accounts for.
#[cfg(not(target_os = "macos"))]
fn schedule_glass_check(_cx: &mut App) {
    println!("--verify-glass chỉ hỗ trợ macOS; các nền tảng khác dùng Chrome::detect()");
}
