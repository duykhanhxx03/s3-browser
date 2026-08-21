//! s3browser — a desktop S3 client built on GPUI.
//!
//! Runs on macOS, Windows and Linux; everything that differs between them is
//! isolated in [`platform`].

// Hide the console window that Windows would otherwise open alongside the GUI.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod assets;
mod browser;
mod crash;
mod failure;
#[cfg(target_os = "macos")]
mod glass_check;
mod platform;
mod theme;

use std::time::Duration;

use gpui::{px, size, App, AppContext as _, Application, Bounds, WindowOptions};

use browser::Browser;
use platform::Chrome;

fn main() {
    // Before anything else: a panic during setup should still leave a report.
    crash::install();

    // Before any runtime or window exists: reading the system timezone is only
    // sound while the process is single-threaded.
    s3core::set_local_offset(s3core::detect_local_offset());

    Application::new()
        .with_assets(assets::Assets)
        .run(|cx: &mut App| {
        gpui_tokio::init(cx);
        // Brings the component library's key bindings and theme with it. The
        // Input widget's editing keys (word-wise delete, select-all, paste) are
        // bindings rather than hardcoded handling, so skipping this leaves an
        // input that looks right and does nothing.
        gpui_component::init(cx);

        // Before any window: a view built while the font is still unregistered
        // measures its text in the fallback and lays out to the wrong widths.
        if let Err(error) = cx
            .text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(assets::UI_FONT)])
        {
            eprintln!("không nạp được font đi kèm: {error}");
        }

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
            |window, cx| {
                // Dialogs, sheets and notifications from the component library
                // render into layers that only exist inside a Root, so the app's
                // own view has to sit under one.
                let browser = cx.new(|cx| Browser::new(window, cx));
                cx.new(|cx| gpui_component::Root::new(gpui::AnyView::from(browser), window, cx))
            },
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
            // Which inputs produced the mode, so a surprising result can be
            // traced to the setting responsible rather than guessed at.
            println!(
                "reduce transparency: {} | chế độ: {:?}",
                platform::reduce_transparency(),
                platform::Chrome::detect()
            );
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
