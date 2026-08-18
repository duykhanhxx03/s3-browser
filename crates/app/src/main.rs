//! M0 feasibility spike for s3browser.
//!
//! Proves the four things the plan gates on before committing to GPUI:
//!   1. a glass window (system blur behind translucent surfaces, custom titlebar),
//!   2. a virtualized list that stays smooth at 100k rows,
//!   3. drag-and-drop of files from Finder, with real paths,
//!   4. the AWS SDK running on Tokio while the UI runs on GPUI's executor.

#[cfg(target_os = "macos")]
mod glass_check;

use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    div, point, prelude::*, px, rgb, rgba, size, uniform_list, App, Application, Bounds, Context,
    ExternalPaths, SharedString, Task, TitlebarOptions, Window, WindowBackgroundAppearance,
    WindowBounds, WindowOptions,
};
use gpui_tokio::Tokio;
use s3core::{format_size, Entry, Profile, S3Client};

/// Row count used by the stress toggle: far beyond any real prefix, so if the
/// list stays smooth here it will stay smooth in production.
const STRESS_ROWS: usize = 100_000;

const ROW_HEIGHT: f32 = 30.;

struct S3Browser {
    status: SharedString,
    client: Option<S3Client>,
    buckets: Vec<SharedString>,
    current_bucket: Option<SharedString>,
    prefix: String,
    entries: Vec<Entry>,
    /// Paths most recently dropped from Finder — M2 turns these into uploads.
    dropped: Vec<PathBuf>,
    stress: bool,
    /// Last row range `uniform_list` asked us to build, logged so the spike can
    /// show that a 100k-row list only ever materializes a screenful.
    last_range: Option<Range<usize>>,
    tasks: Vec<Task<()>>,
}

impl S3Browser {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            status: "Đang kết nối MinIO…".into(),
            client: None,
            buckets: Vec::new(),
            current_bucket: None,
            prefix: String::new(),
            entries: Vec::new(),
            dropped: Vec::new(),
            stress: std::env::args().any(|arg| arg == "--stress"),
            last_range: None,
            tasks: Vec::new(),
        };
        this.connect(cx);
        this
    }

    /// Connects and lists buckets. The SDK future runs on Tokio; the result is
    /// applied back on GPUI's foreground thread.
    fn connect(&mut self, cx: &mut Context<Self>) {
        self.status = "Đang kết nối MinIO…".into();
        self.client = None;

        let connecting = Tokio::spawn(cx, async move {
            let client = S3Client::connect(&Profile::minio_local()).await?;
            let buckets = client.list_buckets().await?;
            anyhow::Ok((client, buckets))
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = connecting.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok((client, buckets))) => {
                        println!("[s3] connected via gpui_tokio, {} buckets: {buckets:?}", buckets.len());
                        this.status = format!("Đã kết nối · {} bucket", buckets.len()).into();
                        this.buckets = buckets.into_iter().map(SharedString::from).collect();
                        this.client = Some(client);
                        if let Some(first) = this.buckets.first().cloned() {
                            this.open(first, String::new(), cx);
                        }
                    }
                    Ok(Err(error)) => this.status = format!("Lỗi kết nối: {error}").into(),
                    Err(error) => this.status = format!("Task lỗi: {error}").into(),
                }
                cx.notify();
            });
        });
        self.tasks.push(task);
    }

    /// Lists one prefix. Only the first page is fetched here; paging through the
    /// continuation token as the user scrolls is M1 work.
    fn open(&mut self, bucket: SharedString, prefix: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };

        self.current_bucket = Some(bucket.clone());
        self.prefix = prefix.clone();
        self.stress = false;
        self.status = format!("Đang tải s3://{bucket}/{prefix}").into();

        let listing = Tokio::spawn(cx, async move {
            client.list_page(&bucket, &prefix, None).await
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = listing.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(page)) => {
                        let more = if page.continuation.is_some() { "+" } else { "" };
                        println!(
                            "[s3] listed {} entries{} ({} folders)",
                            page.entries.len(),
                            more,
                            page.entries.iter().filter(|e| e.is_folder).count()
                        );
                        this.status =
                            format!("{} mục{}", page.entries.len(), more).into();
                        this.entries = page.entries;
                        this.last_range = None;
                    }
                    Ok(Err(error)) => this.status = format!("Lỗi listing: {error}").into(),
                    Err(error) => this.status = format!("Task lỗi: {error}").into(),
                }
                cx.notify();
            });
        });
        self.tasks.push(task);
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        let Some(bucket) = self.current_bucket.clone() else {
            return;
        };
        let parent = self
            .prefix
            .trim_end_matches('/')
            .rsplit_once('/')
            .map(|(head, _)| format!("{head}/"))
            .unwrap_or_default();
        self.open(bucket, parent, cx);
    }

    fn row_count(&self) -> usize {
        if self.stress {
            STRESS_ROWS
        } else {
            self.entries.len()
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.current_bucket.clone();

        div()
            .w(px(210.))
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .pt_10()
            // Translucent panel: the window blur only shows through non-opaque pixels.
            .bg(rgba(0xffffff0a))
            .border_r_1()
            .border_color(rgba(0xffffff14))
            .child(
                div()
                    .px_2()
                    .pb_2()
                    .text_xs()
                    .text_color(rgba(0xffffff66))
                    .child("BUCKETS"),
            )
            .children(self.buckets.iter().cloned().map(|bucket| {
                let is_current = current.as_ref() == Some(&bucket);
                let target = bucket.clone();
                div()
                    .id(SharedString::from(format!("bucket-{bucket}")))
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .text_sm()
                    .text_color(if is_current {
                        rgb(0xffffff)
                    } else {
                        rgb(0xc7cbd4)
                    })
                    .when(is_current, |this| this.bg(rgba(0x5ca8ff33)))
                    .hover(|this| this.bg(rgba(0xffffff14)))
                    .cursor_pointer()
                    .child(bucket)
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.open(target.clone(), String::new(), cx);
                        cx.notify();
                    }))
            }))
    }

    fn render_rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "objects",
            self.row_count(),
            cx.processor(|this, range: Range<usize>, _window, _cx| {
                if this.last_range.as_ref() != Some(&range) {
                    println!(
                        "[list] built rows {}..{} of {} ({} materialized)",
                        range.start,
                        range.end,
                        this.row_count(),
                        range.len()
                    );
                    this.last_range = Some(range.clone());
                }

                range
                    .map(|ix| {
                        if this.stress {
                            synthetic_row(ix)
                        } else {
                            object_row(&this.entries[ix])
                        }
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .h_full()
    }
}

fn synthetic_row(ix: usize) -> gpui::Div {
    row_shell()
        .child(div().w(px(24.)).child("📄"))
        .child(
            div()
                .flex_1()
                .child(SharedString::from(format!("stress-test/object-{ix:06}.bin"))),
        )
        .child(
            div()
                .w(px(90.))
                .text_color(rgba(0xffffff8c))
                .child(SharedString::from(format_size((ix as i64 % 997) * 4096))),
        )
}

fn object_row(entry: &Entry) -> gpui::Div {
    let size_label = if entry.is_folder {
        SharedString::from("—")
    } else {
        SharedString::from(format_size(entry.size))
    };
    let modified = entry
        .last_modified
        .clone()
        .map(|value| value.chars().take(19).collect::<String>())
        .unwrap_or_default();

    row_shell()
        .child(div().w(px(24.)).child(if entry.is_folder {
            "📁"
        } else {
            "📄"
        }))
        .child(
            div()
                .flex_1()
                .text_color(if entry.is_folder {
                    rgb(0xffffff)
                } else {
                    rgb(0xdfe3ea)
                })
                .child(SharedString::from(entry.name.clone())),
        )
        .child(
            div()
                .w(px(90.))
                .text_color(rgba(0xffffff8c))
                .child(size_label),
        )
        .child(
            div()
                .w(px(160.))
                .text_color(rgba(0xffffff66))
                .child(SharedString::from(modified)),
        )
}

fn row_shell() -> gpui::Div {
    div()
        .h(px(ROW_HEIGHT))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .text_color(rgb(0xdfe3ea))
}

impl Render for S3Browser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let location = match &self.current_bucket {
            Some(bucket) => format!("s3://{bucket}/{}", self.prefix),
            None => "chưa kết nối".to_string(),
        };
        let dropped_label = match self.dropped.len() {
            0 => "Kéo file từ Finder thả vào đây".to_string(),
            1 => format!("Đã thả: {}", self.dropped[0].display()),
            n => format!(
                "Đã thả {n} mục · mới nhất: {}",
                self.dropped
                    .last()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default()
            ),
        };
        let stress_label = if self.stress {
            "Về listing thật"
        } else {
            "Stress 100k dòng"
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .font_family("SF Pro Text")
            // Translucent ground so the system blur behind the window shows through.
            .bg(rgba(0x14161cd9))
            .text_color(rgb(0xdfe3ea))
            .child(
                // Titlebar strip: padded left to clear the traffic lights.
                div()
                    .h(px(38.))
                    .flex()
                    .items_center()
                    .gap_3()
                    .pl(px(88.))
                    .pr_3()
                    .border_b_1()
                    .border_color(rgba(0xffffff14))
                    .child(
                        div()
                            .id("up")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_sm()
                            .cursor_pointer()
                            .hover(|this| this.bg(rgba(0xffffff14)))
                            .child("↑")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.go_up(cx);
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1().text_sm().child(SharedString::from(location)))
                    .child(
                        div()
                            .id("stress")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_xs()
                            .cursor_pointer()
                            .bg(rgba(0xffffff14))
                            .hover(|this| this.bg(rgba(0xffffff26)))
                            .child(stress_label)
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.stress = !this.stress;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("reload")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_xs()
                            .cursor_pointer()
                            .bg(rgba(0xffffff14))
                            .hover(|this| this.bg(rgba(0xffffff26)))
                            .child("Kết nối lại")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.connect(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .overflow_hidden()
                    .child(self.render_sidebar(cx))
                    .child(
                        // Drop target: the whole object pane accepts Finder drags.
                        div()
                            .id("object-pane")
                            .flex_1()
                            .h_full()
                            .flex()
                            .flex_col()
                            .drag_over::<ExternalPaths>(|style, _paths, _window, _cx| {
                                style.bg(rgba(0x5ca8ff29))
                            })
                            .on_drop::<ExternalPaths>(cx.listener(
                                |this, paths: &ExternalPaths, _window, cx| {
                                    this.dropped.extend(paths.paths().iter().cloned());
                                    this.status = format!(
                                        "Nhận {} đường dẫn từ Finder",
                                        paths.paths().len()
                                    )
                                    .into();
                                    cx.notify();
                                },
                            ))
                            .child(self.render_rows(cx)),
                    ),
            )
            .child(
                // Status bar — stands in for the transfer drawer built in M2.
                div()
                    .h(px(30.))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .text_xs()
                    .bg(rgba(0xffffff0a))
                    .border_t_1()
                    .border_color(rgba(0xffffff14))
                    .child(
                        div()
                            .text_color(rgba(0xffffffb3))
                            .child(SharedString::from(self.status.clone())),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_color(rgba(0xffffff8c))
                            .child(SharedString::from(dropped_label)),
                    ),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_tokio::init(cx);

        let bounds = Bounds::centered(None, size(px(1080.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("s3browser".into()),
                    // Full-size content view: our own chrome draws under the
                    // traffic lights, which we nudge inward to match the strip.
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.), px(13.))),
                }),
                // The glass: NSVisualEffectView vibrancy behind the window.
                window_background: WindowBackgroundAppearance::Blurred,
                ..Default::default()
            },
            |_window, cx| cx.new(S3Browser::new),
        )
        .expect("failed to open window");

        cx.activate(true);

        if std::env::args().any(|arg| arg == "--verify-glass") {
            schedule_glass_check(cx);
        }
    });
}

/// Waits for the first frames to land, prints what AppKit actually did to the
/// window, then exits with a status that CI can gate on.
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

#[cfg(not(target_os = "macos"))]
fn schedule_glass_check(_cx: &mut App) {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{FileDropEvent, ExternalPaths, TestAppContext};

    /// A view with no S3 connection, so tests exercise the UI in isolation.
    fn offline_view(stress: bool, entries: Vec<Entry>) -> S3Browser {
        S3Browser {
            status: "test".into(),
            client: None,
            buckets: Vec::new(),
            current_bucket: None,
            prefix: String::new(),
            entries,
            dropped: Vec::new(),
            stress,
            last_range: None,
            tasks: Vec::new(),
        }
    }

    /// The gate that decides whether GPUI can browse a bucket with a huge prefix:
    /// asking for 100k rows must still only build a screenful of elements.
    #[gpui::test]
    async fn materializes_only_visible_rows_of_a_100k_list(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, _cx| offline_view(true, Vec::new()));
        cx.run_until_parked();

        let range = view
            .read_with(cx, |this, _| this.last_range.clone())
            .expect("uniform_list should have requested a row range");

        assert_eq!(view.read_with(cx, |this, _| this.row_count()), STRESS_ROWS);
        assert!(
            range.len() < 500,
            "expected a screenful, but {} of {STRESS_ROWS} rows were built",
            range.len()
        );
        assert!(range.start < STRESS_ROWS);
    }

    /// Proves the Finder drop is wired end to end: the platform event reaches the
    /// object pane's hitbox and our listener mutates view state.
    ///
    /// The payload is empty because gpui 0.2.2 keeps `ExternalPaths`' field
    /// crate-private, so no test outside gpui can build one carrying paths; the
    /// platform layer that fills it is exercised by gpui itself.
    #[gpui::test]
    async fn accepts_a_drop_from_finder(cx: &mut TestAppContext) {
        let entries = vec![Entry {
            name: "readme.txt".into(),
            key: "readme.txt".into(),
            is_folder: false,
            size: 21,
            last_modified: None,
            storage_class: None,
        }];
        let (view, cx) = cx.add_window_view(|_window, _cx| offline_view(false, entries));
        cx.run_until_parked();

        // Drop in the middle of the window, which is inside the object pane.
        let center = cx.update(|window, _| {
            let viewport = window.viewport_size();
            point(viewport.width / 2., viewport.height / 2.)
        });

        cx.simulate_event(FileDropEvent::Entered {
            position: center,
            paths: ExternalPaths::default(),
        });
        cx.simulate_event(FileDropEvent::Submit { position: center });
        cx.run_until_parked();

        let status = view.read_with(cx, |this, _| this.status.to_string());
        assert!(
            status.contains("từ Finder"),
            "drop handler did not run; status was {status:?}"
        );
    }
}
