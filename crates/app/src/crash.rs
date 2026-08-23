//! Crash reporting.
//!
//! A panic in a GUI app is invisible: the window disappears and stderr goes
//! nowhere the user will ever look. This writes a report to disk first, so
//! "it just vanished" turns into a file that can be attached to a bug.
//!
//! **What is and is not here.** Local capture is implemented and tested.
//! Uploading to Sentry is deliberately not wired: it needs a DSN belonging to
//! whoever runs the project, and code written against an endpoint nobody can
//! reach cannot be verified — it would look finished while being untested. The
//! report is written in a shape that an uploader can pick up later, and
//! `S3BROWSER_CRASH_DIR` points it somewhere else for testing.

use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::PathBuf;

/// Installs the hook. Call once, early — a panic before this lands nowhere.
pub fn install() {
    // Keep the default hook: it still prints to stderr, which is what a
    // developer running from a terminal expects to see.
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        // Writing must not itself panic, or the process dies inside its own
        // crash handler and the user gets nothing at all.
        if let Some(path) = report_path() {
            let report = format_report(
                info,
                &std::backtrace::Backtrace::force_capture().to_string(),
            );
            _ = write_report(&path, &report);
        }
        previous(info);
    }));
}

/// Where reports go: beside the profiles, so a user asked for "the s3browser
/// folder" hands over both.
pub fn report_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("S3BROWSER_CRASH_DIR") {
        return Some(PathBuf::from(dir));
    }
    Some(dirs::config_dir()?.join("s3browser").join("crashes"))
}

fn report_path() -> Option<PathBuf> {
    // The process id keeps two crashes in one session from overwriting each
    // other; there is no clock here because a panic hook should not depend on
    // anything that can itself fail.
    Some(report_dir()?.join(format!("crash-{}.txt", std::process::id())))
}

/// The report body. Pure so its shape can be tested without crashing anything.
pub fn format_report(info: &PanicHookInfo<'_>, backtrace: &str) -> String {
    let message = panic_message(info);
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "không rõ".into());

    format!(
        "s3browser {}\n\
         nền tảng: {} {}\n\
         vị trí: {location}\n\
         thông điệp: {message}\n\
         \n\
         {backtrace}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

/// Panics carry either a `&str` or a `String`; missing both is possible and
/// must not be reported as an empty line with no explanation.
fn panic_message(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "(payload không phải chuỗi)".to_string()
    }
}

fn write_report(path: &PathBuf, report: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(report.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a real `PanicHookInfo` by catching a panic, because the type
    /// cannot be constructed directly.
    fn with_panic_info(f: impl Fn(&PanicHookInfo<'_>) + Send + Sync + 'static) {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| f(info)));
        _ = std::panic::catch_unwind(|| panic!("bùng nổ có chủ đích"));
        std::panic::set_hook(previous);
    }

    #[test]
    fn report_carries_what_a_bug_report_needs() {
        let (tx, rx) = std::sync::mpsc::channel();
        with_panic_info(move |info| {
            _ = tx.send(format_report(info, "khung 1\nkhung 2"));
        });
        let report = rx.recv().unwrap();

        // The message, or the report says nothing about what went wrong.
        assert!(report.contains("bùng nổ có chủ đích"), "{report}");
        // The source location, which is the first thing anyone looks for.
        assert!(report.contains("crash.rs:"), "{report}");
        // Version and platform: a stack trace without them is guesswork.
        assert!(report.contains(env!("CARGO_PKG_VERSION")), "{report}");
        assert!(report.contains(std::env::consts::OS), "{report}");
        assert!(report.contains("khung 1"), "{report}");
    }

    #[test]
    fn writing_creates_the_directory_it_needs() {
        let dir = std::env::temp_dir().join(format!("s3browser-crash-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("crash.txt");

        // The config directory may not exist on a first run, and a crash
        // handler that fails because of that reports nothing.
        write_report(&path, "xin chào").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xin chào");

        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_dir_can_be_redirected() {
        // Without an override the location depends on the platform's config
        // directory, which a test must not write into.
        std::env::set_var("S3BROWSER_CRASH_DIR", "/tmp/s3browser-test-crashes");
        assert_eq!(
            report_dir().unwrap(),
            PathBuf::from("/tmp/s3browser-test-crashes")
        );
        std::env::remove_var("S3BROWSER_CRASH_DIR");
    }
}
