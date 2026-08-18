//! The browser window: profiles and buckets on the left, one prefix listed on
//! the right, with sorting, filtering, paging and folder operations.

use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, uniform_list, ClickEvent, Context, ExternalPaths, FocusHandle, KeyDownEvent,
    Modifiers, SharedString, Subscription, Task, UniformListScrollHandle, Window,
};
use gpui_tokio::Tokio;
use s3core::{format_size, format_timestamp, sort_entries, Entry, Profile, S3Client, Sort, SortKey};
use transfer::{Job, JobState, TransferEngine};
use vault::{ImportedProfile, ProfileStore, StoredProfile};

use crate::platform::{self, Chrome};
use crate::theme::Theme;

/// Diagnostic logging, enabled with `S3BROWSER_DEBUG=1`. Kept out of the normal
/// run so a shipped build stays quiet, but available when a user reports that a
/// provider behaves oddly.
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var_os("S3BROWSER_DEBUG").is_some() {
            println!("[s3browser] {}", format!($($arg)*));
        }
    };
}

const ROW_HEIGHT: f32 = 28.;
const SIDEBAR_WIDTH: f32 = 214.;
/// Start fetching the next page once the viewport comes this close to the end.
const PREFETCH_MARGIN: usize = 40;

/// What typed characters currently go to. One mechanism covers the filter box
/// and the two name prompts, so there is a single place that handles Enter,
/// Escape and backspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    Filter,
    NewFolder,
    NewBucket,
}

impl Prompt {
    fn label(&self) -> &'static str {
        match self {
            Prompt::Filter => "Lọc",
            Prompt::NewFolder => "Tên thư mục mới",
            Prompt::NewBucket => "Tên bucket mới",
        }
    }
}

pub struct Browser {
    focus: FocusHandle,
    theme: Theme,
    chrome: Chrome,

    profiles: Vec<StoredProfile>,
    active_profile: Option<usize>,
    store: Option<ProfileStore>,

    client: Option<S3Client>,
    buckets: Vec<SharedString>,
    bucket: Option<SharedString>,
    prefix: String,

    /// Everything fetched so far for the current prefix, already sorted.
    entries: Vec<Entry>,
    /// Indices into `entries` that survive the filter — what the list renders.
    visible: Vec<usize>,
    continuation: Option<String>,
    loading: bool,
    loading_more: bool,
    /// Bumped on every navigation so a late response for an abandoned prefix is
    /// dropped instead of overwriting the current listing.
    generation: u64,

    sort: Sort,
    filter: String,
    prompt: Option<Prompt>,
    prompt_text: String,
    selection: HashSet<String>,

    scroll: UniformListScrollHandle,
    status: SharedString,
    error: Option<SharedString>,

    transfers: TransferEngine,
    drawer_open: bool,
    /// True while a repaint loop is running to animate transfer progress.
    ticking: bool,

    /// Named slots rather than a growing vec: replacing the listing task cancels
    /// the request it superseded, and nothing accumulates for the session.
    connect_task: Option<Task<()>>,
    listing_task: Option<Task<()>>,
    paging_task: Option<Task<()>>,
    op_task: Option<Task<()>>,
    tick_task: Option<Task<()>>,
    _appearance: Option<Subscription>,
}

impl Browser {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let chrome = Chrome::detect();
        let theme = Theme::from_window(window.appearance(), chrome);

        // Follow the system between light and dark without a restart.
        let weak = cx.entity().downgrade();
        let appearance = window.observe_window_appearance(move |window, cx| {
            let appearance = window.appearance();
            _ = weak.update(cx, |this: &mut Self, cx| {
                this.theme = Theme::from_window(appearance, this.chrome);
                cx.notify();
            });
        });

        let store = ProfileStore::default_location().ok();
        let profiles = store
            .as_ref()
            .and_then(|store| store.load().ok())
            .unwrap_or_default();

        // The queue lives next to the profiles so unfinished transfers come back
        // after a restart.
        let transfers = store
            .as_ref()
            .and_then(|store| store.path().parent().map(|dir| dir.join("transfers.db")))
            .and_then(|path| TransferEngine::open(&path).ok())
            .or_else(|| TransferEngine::in_memory().ok())
            .expect("an in-memory queue always opens");

        let mut this = Self {
            focus: cx.focus_handle(),
            theme,
            chrome,
            profiles,
            active_profile: None,
            store,
            client: None,
            buckets: Vec::new(),
            bucket: None,
            prefix: String::new(),
            entries: Vec::new(),
            visible: Vec::new(),
            continuation: None,
            loading: false,
            loading_more: false,
            generation: 0,
            sort: Sort::default(),
            filter: String::new(),
            prompt: None,
            prompt_text: String::new(),
            selection: HashSet::new(),
            scroll: UniformListScrollHandle::new(),
            status: "Chọn một profile để bắt đầu".into(),
            error: None,
            transfers,
            drawer_open: false,
            ticking: false,
            connect_task: None,
            listing_task: None,
            paging_task: None,
            op_task: None,
            tick_task: None,
            _appearance: Some(appearance),
        };

        if !this.profiles.is_empty() {
            this.connect(0, cx);
        }
        this
    }

    // ---------------------------------------------------------------- profiles

    fn save_profiles(&mut self) {
        if let (Some(store), Err(error)) = (
            self.store.as_ref(),
            self.store
                .as_ref()
                .map(|store| store.save(&self.profiles))
                .unwrap_or(Ok(())),
        ) {
            self.error = Some(format!("Không lưu được {}: {error}", store.path().display()).into());
        }
    }

    fn add_profile(&mut self, profile: StoredProfile, secret: &str, cx: &mut Context<Self>) {
        if let Err(error) = vault::set_secret_key(&profile.id, secret) {
            self.error = Some(format!("Không lưu được khoá bí mật: {error}").into());
            return;
        }
        self.profiles.push(profile);
        self.save_profiles();
        self.connect(self.profiles.len() - 1, cx);
    }

    fn add_minio_dev_profile(&mut self, cx: &mut Context<Self>) {
        let id = vault::new_profile_id("MinIO local", &self.profiles);
        let profile = StoredProfile {
            id,
            name: "MinIO local".into(),
            endpoint: Some("http://127.0.0.1:9000".into()),
            region: "us-east-1".into(),
            path_style: true,
            relaxed_checksums: true,
            access_key: "minioadmin".into(),
        };
        self.add_profile(profile, "minioadmin", cx);
    }

    fn import_from_aws(&mut self, cx: &mut Context<Self>) {
        let imported = match vault::import_aws_profiles() {
            Ok(imported) => imported,
            Err(error) => {
                self.error = Some(format!("Không đọc được ~/.aws: {error}").into());
                return;
            }
        };

        if imported.is_empty() {
            self.status = "Không tìm thấy profile nào có khoá tĩnh trong ~/.aws".into();
            cx.notify();
            return;
        }

        let mut added = 0;
        for ImportedProfile {
            mut profile,
            secret_key,
        } in imported
        {
            if self.profiles.iter().any(|p| p.name == profile.name) {
                continue;
            }
            profile.id = vault::new_profile_id(&profile.name, &self.profiles);
            if let Err(error) = vault::set_secret_key(&profile.id, &secret_key) {
                self.error = Some(format!("Không lưu được khoá cho {}: {error}", profile.name).into());
                continue;
            }
            self.profiles.push(profile);
            added += 1;
        }

        self.save_profiles();
        self.status = format!("Đã nhập {added} profile từ ~/.aws").into();
        if added > 0 && self.client.is_none() {
            self.connect(self.profiles.len() - added, cx);
        }
        cx.notify();
    }

    fn connect(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(stored) = self.profiles.get(index).cloned() else {
            return;
        };

        let secret = match vault::secret_key(&stored.id) {
            Ok(secret) => secret,
            Err(error) => {
                self.error = Some(format!("Không đọc được khoá bí mật: {error}").into());
                cx.notify();
                return;
            }
        };

        self.active_profile = Some(index);
        self.client = None;
        self.buckets.clear();
        self.bucket = None;
        self.entries.clear();
        self.visible.clear();
        self.error = None;
        self.status = format!("Đang kết nối {}…", stored.name).into();

        let profile = Profile {
            name: stored.name.clone(),
            endpoint: stored.endpoint.clone(),
            region: stored.region.clone(),
            path_style: stored.path_style,
            access_key: stored.access_key.clone(),
            secret_key: secret,
            relaxed_checksums: stored.relaxed_checksums,
        };

        let connecting = Tokio::spawn(cx, async move {
            let client = S3Client::connect(&profile).await?;
            let buckets = client.list_buckets().await?;
            anyhow::Ok((client, buckets))
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = connecting.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok((client, buckets))) => {
                        debug_log!("connected: {} buckets {:?}", buckets.len(), buckets);
                        this.status = format!("{} bucket", buckets.len()).into();
                        this.buckets = buckets.into_iter().map(SharedString::from).collect();
                        this.client = Some(client);
                        // `--open bucket/prefix/` jumps straight to a location,
                        // which keeps deep prefixes reachable from a script.
                        match requested_location() {
                            Some((bucket, prefix)) => this.open(bucket.into(), prefix, cx),
                            None => {
                                if let Some(first) = this.buckets.first().cloned() {
                                    this.open(first, String::new(), cx);
                                }
                            }
                        }
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        });
        self.connect_task = Some(task);
        cx.notify();
    }

    /// Surfaces a failure in the status bar and on stderr, so a user who runs
    /// from a terminal sees it even after the next action clears the bar.
    fn report(&mut self, message: String) {
        eprintln!("[s3browser] error: {message}");
        self.error = Some(message.into());
    }

    // -------------------------------------------------------------- navigation

    pub fn open(&mut self, bucket: SharedString, prefix: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };

        self.generation += 1;
        let generation = self.generation;

        self.bucket = Some(bucket.clone());
        self.prefix = prefix.clone();
        self.entries.clear();
        self.visible.clear();
        self.selection.clear();
        self.continuation = None;
        self.loading = true;
        self.error = None;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);

        let listing =
            Tokio::spawn(cx, async move { client.list_page(&bucket, &prefix, None).await });

        let task = cx.spawn(async move |this, cx| {
            let outcome = listing.await;
            _ = this.update(cx, |this, cx| {
                // A newer navigation started while this request was in flight.
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match outcome {
                    Ok(Ok(page)) => {
                        debug_log!(
                            "listed {} entries, more={}",
                            page.entries.len(),
                            page.continuation.is_some()
                        );
                        this.entries = page.entries;
                        this.continuation = page.continuation;
                        this.resort_and_filter();
                        this.status = this.listing_summary();
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        });
        self.listing_task = Some(task);
        cx.notify();
    }

    /// Fetches the next page and appends it. Called from the list processor when
    /// the viewport approaches the end of what we have.
    fn load_more(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(token)) = (
            self.client.clone(),
            self.bucket.clone(),
            self.continuation.clone(),
        ) else {
            return;
        };
        if self.loading_more {
            return;
        }

        self.loading_more = true;
        let generation = self.generation;
        let prefix = self.prefix.clone();

        let listing = Tokio::spawn(cx, async move {
            client.list_page(&bucket, &prefix, Some(token)).await
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = listing.await;
            _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading_more = false;
                match outcome {
                    Ok(Ok(page)) => {
                        debug_log!("page +{} entries", page.entries.len());
                        this.entries.extend(page.entries);
                        this.continuation = page.continuation;
                        this.resort_and_filter();
                        this.status = this.listing_summary();
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        });
        self.paging_task = Some(task);
    }

    fn listing_summary(&self) -> SharedString {
        let total = self.entries.len();
        let shown = self.visible.len();
        let more = if self.continuation.is_some() { "+" } else { "" };
        if shown == total {
            format!("{total}{more} mục").into()
        } else {
            format!("{shown}/{total}{more} mục").into()
        }
    }

    fn enter(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(entry_index) else {
            return;
        };
        if !entry.is_folder {
            return;
        }
        let (Some(bucket), key) = (self.bucket.clone(), entry.key.clone()) else {
            return;
        };
        self.open(bucket, key, cx);
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        let Some(bucket) = self.bucket.clone() else {
            return;
        };
        let parent = parent_prefix(&self.prefix);
        if parent == self.prefix {
            return;
        }
        self.open(bucket, parent, cx);
    }

    /// `photos/2026/` → [("photos", "photos/"), ("2026", "photos/2026/")]
    fn breadcrumbs(&self) -> Vec<(SharedString, String)> {
        let mut crumbs = Vec::new();
        let mut accumulated = String::new();
        for segment in self.prefix.split('/').filter(|s| !s.is_empty()) {
            accumulated.push_str(segment);
            accumulated.push('/');
            crumbs.push((SharedString::from(segment.to_string()), accumulated.clone()));
        }
        crumbs
    }

    // ------------------------------------------------------------ sort/filter

    fn resort_and_filter(&mut self) {
        sort_entries(&mut self.entries, self.sort);

        let needle = self.filter.to_lowercase();
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| needle.is_empty() || entry.name.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect();
    }

    fn toggle_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.sort = self.sort.toggled(key);
        self.resort_and_filter();
        self.status = self.listing_summary();
        cx.notify();
    }

    // ------------------------------------------------------------- operations

    fn create_folder(&mut self, name: String, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        let prefix = self.prefix.clone();
        let reopen = (bucket.clone(), prefix.clone());

        let creating = Tokio::spawn(cx, async move {
            client.create_folder(&bucket, &prefix, &name).await
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = creating.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(_)) => this.open(reopen.0, reopen.1, cx),
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        });
        self.op_task = Some(task);
    }

    fn create_bucket(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let creating = Tokio::spawn(cx, async move {
            client.create_bucket(&name).await?;
            let buckets = client.list_buckets().await?;
            anyhow::Ok(buckets)
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = creating.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(buckets)) => {
                        this.buckets = buckets.into_iter().map(SharedString::from).collect();
                        this.status = format!("{} bucket", this.buckets.len()).into();
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        });
        self.op_task = Some(task);
    }

    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        let doomed: Vec<Entry> = self
            .entries
            .iter()
            .filter(|entry| self.selection.contains(&entry.key))
            .cloned()
            .collect();
        if doomed.is_empty() {
            return;
        }

        self.status = format!("Đang xoá {} mục…", doomed.len()).into();
        let reopen = (bucket.clone(), self.prefix.clone());

        let deleting = Tokio::spawn(cx, async move {
            client.delete_entries(&bucket, &doomed).await
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = deleting.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(report)) => {
                        if report.errors.is_empty() {
                            this.status = format!("Đã xoá {} key", report.deleted).into();
                        } else {
                            this.error = Some(
                                format!(
                                    "Xoá {} key, {} lỗi: {}",
                                    report.deleted,
                                    report.errors.len(),
                                    report.errors.join("; ")
                                )
                                .into(),
                            );
                        }
                        this.open(reopen.0, reopen.1, cx);
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        });
        self.op_task = Some(task);
        cx.notify();
    }

    // -------------------------------------------------------------- transfers

    /// Queues everything dropped from the file manager into the open prefix.
    fn start_uploads(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            self.report("Chưa mở bucket nào để tải lên".into());
            return;
        };

        let engine = self.transfers.clone();
        let prefix = self.prefix.clone();
        let count = paths.len();
        self.drawer_open = true;
        self.status = format!("Đang xếp {count} mục vào hàng đợi…").into();

        let queueing = Tokio::spawn(cx, async move {
            engine
                .enqueue_uploads(client, &bucket, &prefix, &paths)
                .await
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = queueing.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(ids)) => {
                        debug_log!("queued {} uploads", ids.len());
                        this.status = format!("Đã xếp {} tệp vào hàng đợi", ids.len()).into();
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                this.start_ticking(cx);
                cx.notify();
            });
        });
        self.op_task = Some(task);
        cx.notify();
    }

    /// Downloads the selected objects into the platform's Downloads folder.
    fn download_selection(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        let keys: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| !entry.is_folder && self.selection.contains(&entry.key))
            .map(|entry| entry.key.clone())
            .collect();
        if keys.is_empty() {
            self.report("Chọn tệp để tải xuống (thư mục chưa hỗ trợ)".into());
            return;
        }

        let Some(destination) = dirs::download_dir().or_else(dirs::home_dir) else {
            self.report("Không tìm được thư mục Downloads".into());
            return;
        };

        let engine = self.transfers.clone();
        self.drawer_open = true;

        let queueing = Tokio::spawn(cx, async move {
            let mut ids = Vec::new();
            for key in keys {
                ids.push(
                    engine
                        .enqueue_download(client.clone(), &bucket, &key, &destination)
                        .await?,
                );
            }
            anyhow::Ok(ids)
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = queueing.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(ids)) => {
                        this.status = format!("Đang tải xuống {} tệp", ids.len()).into()
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                this.start_ticking(cx);
                cx.notify();
            });
        });
        self.op_task = Some(task);
        cx.notify();
    }

    /// Repaints while transfers move, so progress bars animate. The loop stops
    /// itself once the queue goes quiet rather than burning a timer forever.
    fn start_ticking(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;

        let executor = cx.background_executor().clone();
        self.tick_task = Some(cx.spawn(async move |this, cx| {
            loop {
                executor.timer(Duration::from_millis(250)).await;
                let still_working = this.update(cx, |this, cx| {
                    cx.notify();
                    this.transfers.has_active_work()
                });
                match still_working {
                    Ok(true) => continue,
                    // Entity gone, or the queue is idle.
                    _ => break,
                }
            }
            _ = this.update(cx, |this, cx| {
                this.ticking = false;
                cx.notify();
            });
        }));
    }

    // ------------------------------------------------------------------ input

    fn start_prompt(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        self.prompt_text = if prompt == Prompt::Filter {
            self.filter.clone()
        } else {
            String::new()
        };
        self.prompt = Some(prompt);
        cx.notify();
    }

    fn commit_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let text = std::mem::take(&mut self.prompt_text);
        match prompt {
            Prompt::Filter => {} // already applied while typing
            Prompt::NewFolder if !text.trim().is_empty() => {
                self.create_folder(text.trim().to_string(), cx)
            }
            Prompt::NewBucket if !text.trim().is_empty() => {
                self.create_bucket(text.trim().to_string(), cx)
            }
            _ => {}
        }
        cx.notify();
    }

    fn cancel_prompt(&mut self, cx: &mut Context<Self>) {
        if self.prompt == Some(Prompt::Filter) {
            self.filter.clear();
            self.resort_and_filter();
            self.status = self.listing_summary();
        }
        self.prompt = None;
        self.prompt_text.clear();
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let primary = is_primary(&keystroke.modifiers);

        if primary {
            match keystroke.key.as_str() {
                "f" => return self.start_prompt(Prompt::Filter, cx),
                "n" if keystroke.modifiers.shift => {
                    return self.start_prompt(Prompt::NewBucket, cx)
                }
                "n" => return self.start_prompt(Prompt::NewFolder, cx),
                "r" => {
                    if let (Some(bucket), prefix) = (self.bucket.clone(), self.prefix.clone()) {
                        self.open(bucket, prefix, cx);
                    }
                    return;
                }
                "d" => return self.download_selection(cx),
                "j" => {
                    self.drawer_open = !self.drawer_open;
                    cx.notify();
                    return;
                }
                "backspace" | "delete" => return self.delete_selection(cx),
                "up" => return self.go_up(cx),
                _ => {}
            }
        }

        if self.prompt.is_some() {
            match keystroke.key.as_str() {
                "escape" => return self.cancel_prompt(cx),
                "enter" => return self.commit_prompt(cx),
                "backspace" => {
                    self.prompt_text.pop();
                }
                _ => {
                    // `key_char` is what the platform's layout produced, so this
                    // handles non-US layouts and accented characters correctly.
                    match keystroke.key_char.as_deref() {
                        Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => {
                            self.prompt_text.push_str(text)
                        }
                        _ => return,
                    }
                }
            }

            if self.prompt == Some(Prompt::Filter) {
                self.filter = self.prompt_text.clone();
                self.resort_and_filter();
                self.status = self.listing_summary();
            }
            cx.notify();
            return;
        }

        if keystroke.key == "escape" && !self.filter.is_empty() {
            self.filter.clear();
            self.resort_and_filter();
            self.status = self.listing_summary();
            cx.notify();
        }
    }

    fn click_row(&mut self, entry_index: usize, modifiers: Modifiers, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(entry_index) else {
            return;
        };
        let key = entry.key.clone();

        if is_primary(&modifiers) {
            if !self.selection.remove(&key) {
                self.selection.insert(key);
            }
        } else {
            self.selection.clear();
            self.selection.insert(key);
        }
        cx.notify();
    }

    // ----------------------------------------------------------------- render

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let active_profile = self.active_profile;
        let current_bucket = self.bucket.clone();

        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .bg(theme.panel)
            .border_r_1()
            .border_color(theme.border)
            .child(section_label("PROFILES", theme))
            .children(
                self.profiles
                    .iter()
                    .enumerate()
                    .map(|(index, profile)| {
                        let selected = active_profile == Some(index);
                        sidebar_row(
                            SharedString::from(format!("profile-{}", profile.id)),
                            SharedString::from(profile.name.clone()),
                            selected,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.connect(index, cx);
                        }))
                    })
                    .collect::<Vec<_>>(),
            )
            .when(self.profiles.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            action_button("import-aws", "Nhập từ ~/.aws", theme).on_click(
                                cx.listener(|this, _event, _window, cx| this.import_from_aws(cx)),
                            ),
                        )
                        .child(
                            action_button("add-minio", "Thêm MinIO local", theme).on_click(
                                cx.listener(|this, _event, _window, cx| {
                                    this.add_minio_dev_profile(cx)
                                }),
                            ),
                        ),
                )
            })
            .child(div().h(px(8.)))
            .child(section_label("BUCKETS", theme))
            .children(
                self.buckets
                    .iter()
                    .cloned()
                    .map(|bucket| {
                        let selected = current_bucket.as_ref() == Some(&bucket);
                        let target = bucket.clone();
                        sidebar_row(
                            SharedString::from(format!("bucket-{bucket}")),
                            bucket,
                            selected,
                            theme,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.open(target.clone(), String::new(), cx);
                        }))
                    })
                    .collect::<Vec<_>>(),
            )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let bucket = self.bucket.clone();

        div()
            .h(px(40.))
            .flex()
            .items_center()
            .gap_1()
            .pl(px(platform::toolbar_leading_inset()))
            .pr_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                icon_button("up", "↑", theme)
                    .on_click(cx.listener(|this, _event, _window, cx| this.go_up(cx))),
            )
            .child(
                icon_button("refresh", "⟳", theme).on_click(cx.listener(
                    |this, _event, _window, cx| {
                        if let (Some(bucket), prefix) = (this.bucket.clone(), this.prefix.clone()) {
                            this.open(bucket, prefix, cx);
                        }
                    },
                )),
            )
            // Breadcrumb: bucket name, then one segment per prefix level.
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .overflow_hidden()
                    .when_some(bucket.clone(), |this, bucket| {
                        let target = bucket.clone();
                        this.child(
                            crumb(SharedString::from(format!("crumb-root")), bucket, theme)
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    this.open(target.clone(), String::new(), cx);
                                })),
                        )
                    })
                    .children(self.breadcrumbs().into_iter().map(|(name, prefix)| {
                        let bucket = bucket.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap_0p5()
                            .child(div().text_color(theme.text_faint).text_sm().child("/"))
                            .child(
                                crumb(
                                    SharedString::from(format!("crumb-{prefix}")),
                                    name,
                                    theme,
                                )
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    if let Some(bucket) = bucket.clone() {
                                        this.open(bucket, prefix.clone(), cx);
                                    }
                                })),
                            )
                    })),
            )
            .when(!self.filter.is_empty() && self.prompt.is_none(), |this| {
                this.child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .bg(theme.selected)
                        .text_xs()
                        .text_color(theme.text)
                        .child(SharedString::from(format!("lọc: {}", self.filter))),
                )
            })
            .child(
                action_button("new-folder", "Thư mục mới", theme).on_click(cx.listener(
                    |this, _event, _window, cx| this.start_prompt(Prompt::NewFolder, cx),
                )),
            )
            .when(!self.selection.is_empty(), |this| {
                let count = self.selection.len();
                this.child(
                    action_button("download", "Tải xuống", theme).on_click(cx.listener(
                        |this, _event, _window, cx| this.download_selection(cx),
                    )),
                )
                .child(
                    danger_button("delete", SharedString::from(format!("Xoá {count}")), theme)
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.delete_selection(cx)
                        })),
                )
            })
    }

    fn render_columns(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let sort = self.sort;

        let header = |key: SortKey, label: &'static str| {
            let arrow = if sort.key == key {
                if sort.ascending {
                    " ▲"
                } else {
                    " ▼"
                }
            } else {
                ""
            };
            div()
                .id(SharedString::from(format!("col-{label}")))
                .cursor_pointer()
                .text_xs()
                .text_color(if sort.key == key {
                    theme.text
                } else {
                    theme.text_faint
                })
                .hover(|this| this.text_color(theme.text))
                .child(SharedString::from(format!("{label}{arrow}")))
        };

        div()
            .h(px(26.))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .border_b_1()
            .border_color(theme.border_strong)
            .child(div().w(px(22.)))
            .child(
                div().flex_1().child(header(SortKey::Name, "Tên").on_click(
                    cx.listener(|this, _event, _window, cx| this.toggle_sort(SortKey::Name, cx)),
                )),
            )
            .child(
                div().w(px(84.)).child(header(SortKey::Size, "Kích thước").on_click(
                    cx.listener(|this, _event, _window, cx| this.toggle_sort(SortKey::Size, cx)),
                )),
            )
            .child(
                div()
                    .w(px(132.))
                    .child(header(SortKey::Modified, "Sửa đổi").on_click(cx.listener(
                        |this, _event, _window, cx| this.toggle_sort(SortKey::Modified, cx),
                    ))),
            )
    }

    fn render_rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        uniform_list(
            "objects",
            self.visible.len(),
            cx.processor(move |this: &mut Self, range: Range<usize>, _window, cx| {
                this.maybe_prefetch(&range, cx);

                range
                    .map(|position| {
                        let entry_index = this.visible[position];
                        let entry = &this.entries[entry_index];
                        let selected = this.selection.contains(&entry.key);
                        let is_folder = entry.is_folder;

                        object_row(position, entry, selected, theme).on_click(cx.listener(
                            move |this, event: &ClickEvent, _window, cx| {
                                // gpui 0.2.2 has no `on_double_click`, but the click
                                // event carries the count, so both gestures live here.
                                if is_folder && click_count(event) >= 2 {
                                    this.enter(entry_index, cx);
                                } else {
                                    this.click_row(entry_index, event.modifiers(), cx);
                                }
                            },
                        ))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.scroll.clone())
        .h_full()
    }

    /// Requests the next page once the viewport nears the end of what we hold,
    /// so scrolling through a large prefix never stops at the 1000-key page
    /// boundary S3 imposes.
    fn maybe_prefetch(&mut self, range: &Range<usize>, cx: &mut Context<Self>) {
        if should_prefetch(
            range,
            self.visible.len(),
            self.continuation.is_some(),
            self.loading_more,
        ) {
            self.load_more(cx);
        }
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let stats = self.transfers.stats();
        let queue_label = if stats.active + stats.queued > 0 {
            let speed = if stats.bytes_per_second > 0 {
                format!(" · {}/s", format_size(stats.bytes_per_second as i64))
            } else {
                String::new()
            };
            format!(
                "{} đang chạy, {} chờ{speed}",
                stats.active, stats.queued
            )
        } else if stats.failed > 0 {
            format!("{} lỗi", stats.failed)
        } else if stats.done > 0 {
            format!("{} xong", stats.done)
        } else {
            String::new()
        };

        div()
            .h(px(26.))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .text_xs()
            .bg(theme.panel)
            .border_t_1()
            .border_color(theme.border)
            .child(match &self.error {
                Some(error) => div().text_color(theme.danger).child(error.clone()),
                None => div().text_color(theme.text_muted).child(self.status.clone()),
            })
            .child(div().flex_1())
            .child(
                div()
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!(
                        "{m}F lọc · {m}N thư mục · {m}D tải xuống · {m}J hàng đợi",
                        m = platform::primary_modifier()
                    ))),
            )
            .when(self.loading || self.loading_more, |this| {
                this.child(div().text_color(theme.accent).child("đang tải…"))
            })
            .when(!queue_label.is_empty(), |this| {
                let open = self.drawer_open;
                this.child(
                    div()
                        .id("queue-toggle")
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(if open { theme.selected } else { theme.hover })
                        .text_color(theme.text)
                        .hover(|this| this.bg(theme.selected))
                        .child(SharedString::from(format!(
                            "{} {queue_label}",
                            if open { "▾" } else { "▴" }
                        )))
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.drawer_open = !this.drawer_open;
                            cx.notify();
                        })),
                )
            })
    }

    /// The transfer queue. Collapsed by default; the status bar toggles it.
    fn render_drawer(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.drawer_open {
            return None;
        }
        let theme = self.theme;
        let jobs = self.transfers.snapshot();

        Some(
            div()
                .h(px(168.))
                .flex()
                .flex_col()
                .bg(theme.panel)
                .border_t_1()
                .border_color(theme.border_strong)
                .child(
                    div()
                        .h(px(26.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .text_xs()
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!("HÀNG ĐỢI · {}", jobs.len())))
                        .child(div().flex_1())
                        .child(
                            action_button("clear-finished", "Xoá mục đã xong", theme).on_click(
                                cx.listener(|this, _event, _window, cx| {
                                    this.transfers.clear_finished();
                                    cx.notify();
                                }),
                            ),
                        ),
                )
                .child(if jobs.is_empty() {
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(theme.text_faint)
                        .child("Kéo tệp vào danh sách để tải lên")
                        .into_any_element()
                } else {
                    uniform_list(
                        "transfers",
                        jobs.len(),
                        cx.processor(move |this, range: Range<usize>, _window, cx| {
                            let jobs = this.transfers.snapshot();
                            range
                                .filter_map(|ix| jobs.get(ix).cloned())
                                .map(|job| this.render_job(job, cx))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .flex_1()
                    .into_any_element()
                }),
        )
    }

    fn render_job(&self, job: Job, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let id = job.id;
        let fraction = job.fraction();
        let arrow = match job.direction {
            transfer::Direction::Upload => "↑",
            transfer::Direction::Download => "↓",
        };
        let (state_label, state_color) = match job.state {
            JobState::Queued => ("chờ", theme.text_faint),
            JobState::Running => ("đang chạy", theme.accent),
            JobState::Paused => ("tạm dừng", theme.text_muted),
            JobState::Done => ("xong", theme.text_muted),
            JobState::Failed => ("lỗi", theme.danger),
            JobState::Canceled => ("đã huỷ", theme.text_faint),
        };

        div()
            .id(("job", id as usize))
            .h(px(38.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .text_xs()
            .child(div().w(px(14.)).text_color(theme.text_faint).child(arrow))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_color(theme.text)
                                    .child(SharedString::from(job.display_name())),
                            )
                            .child(
                                div()
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(format!(
                                        "{} / {}",
                                        format_size(job.transferred as i64),
                                        format_size(job.size as i64)
                                    ))),
                            ),
                    )
                    // Progress bar: an outer track with an inner fill sized by
                    // the completed fraction.
                    .child(
                        div()
                            .h(px(4.))
                            .w_full()
                            .rounded_sm()
                            .bg(theme.hover)
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative(fraction))
                                    .rounded_sm()
                                    .bg(if job.state == JobState::Failed {
                                        theme.danger
                                    } else {
                                        theme.accent
                                    }),
                            ),
                    ),
            )
            .child(
                div()
                    .w(px(64.))
                    .text_color(state_color)
                    .child(state_label),
            )
            .child(match job.state {
                JobState::Running | JobState::Queued => action_button("pause", "Tạm dừng", theme)
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.transfers.pause(id);
                        cx.notify();
                    }))
                    .into_any_element(),
                JobState::Paused | JobState::Failed => action_button("resume", "Tiếp tục", theme)
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        if let Some(client) = this.client.clone() {
                            this.transfers.resume(id, client);
                            this.start_ticking(cx);
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
                _ => div().into_any_element(),
            })
            .child(
                icon_button("remove", "✕", theme).on_click(cx.listener(
                    move |this, _event, _window, cx| {
                        this.transfers.remove_job(id);
                        cx.notify();
                    },
                )),
            )
    }

    fn render_prompt(&self) -> Option<impl IntoElement> {
        let theme = self.theme;
        let prompt = self.prompt.as_ref()?;

        Some(
            div()
                .h(px(32.))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .bg(theme.selected)
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child(prompt.label()),
                )
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(theme.text)
                        // A block cursor keeps it obvious that typing goes here,
                        // since this is a key-capture field, not a real input.
                        .child(SharedString::from(format!("{}▏", self.prompt_text))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_faint)
                        .child("Enter để xác nhận · Esc để huỷ"),
                ),
        )
    }
}

impl Render for Browser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        div()
            .id("root")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .flex_col()
            .font_family(platform::ui_font_candidates()[0])
            .bg(theme.ground)
            .text_color(theme.text)
            .child(self.render_toolbar(cx))
            .children(self.render_prompt())
            .child(
                div()
                    .flex_1()
                    .flex()
                    .overflow_hidden()
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .id("object-pane")
                            .flex_1()
                            .h_full()
                            .flex()
                            .flex_col()
                            .drag_over::<ExternalPaths>(move |style, _paths, _window, _cx| {
                                style.bg(theme.drop_target)
                            })
                            .on_drop::<ExternalPaths>(cx.listener(
                                |this, paths: &ExternalPaths, _window, cx| {
                                    this.start_uploads(paths.paths().to_vec(), cx);
                                },
                            ))
                            .child(self.render_columns(cx))
                            .child(self.render_rows(cx)),
                    ),
            )
            .children(self.render_drawer(cx))
            .child(self.render_status(cx))
    }
}

// ------------------------------------------------------------------ elements

fn section_label(text: &'static str, theme: Theme) -> impl IntoElement {
    div()
        .px_2()
        .py_1()
        .text_xs()
        .text_color(theme.text_faint)
        .child(text)
}

fn sidebar_row(
    id: SharedString,
    label: SharedString,
    selected: bool,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .text_color(if selected { theme.text } else { theme.text_muted })
        .when(selected, |this| this.bg(theme.selected))
        .hover(|this| this.bg(theme.hover))
        .child(label)
}

fn action_button(id: &'static str, label: &'static str, theme: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .bg(theme.hover)
        .text_color(theme.text)
        .hover(|this| this.bg(theme.selected))
        .child(label)
}

fn danger_button(id: &'static str, label: SharedString, theme: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .bg(theme.danger)
        .text_color(theme.text_on_accent)
        .child(label)
}

fn icon_button(id: &'static str, glyph: &'static str, theme: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w(px(24.))
        .h(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .text_color(theme.text_muted)
        .hover(|this| this.bg(theme.hover).text_color(theme.text))
        .child(glyph)
}

fn crumb(id: SharedString, label: SharedString, theme: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_1p5()
        .py_0p5()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .text_color(theme.text)
        .hover(|this| this.bg(theme.hover))
        .child(label)
}

fn object_row(
    position: usize,
    entry: &Entry,
    selected: bool,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    let size_label = if entry.is_folder {
        SharedString::from("—")
    } else {
        SharedString::from(format_size(entry.size))
    };
    let modified = entry
        .modified_epoch
        .map(format_timestamp)
        .unwrap_or_default();

    div()
        .id(position)
        .h(px(ROW_HEIGHT))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .cursor_pointer()
        .when(selected, |this| this.bg(theme.selected))
        .hover(|this| this.bg(theme.hover))
        .child(div().w(px(22.)).child(if entry.is_folder {
            "📁"
        } else {
            "📄"
        }))
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .text_color(theme.text)
                .child(SharedString::from(entry.name.clone())),
        )
        .child(
            div()
                .w(px(84.))
                .text_color(theme.text_muted)
                .child(size_label),
        )
        .child(
            div()
                .w(px(132.))
                .text_color(theme.text_faint)
                .child(SharedString::from(modified)),
        )
}

// ------------------------------------------------------------------- helpers

/// Whether the visible range is close enough to the end to justify fetching the
/// next page. Pure so the paging rule can be tested without a client.
fn should_prefetch(
    visible_range: &Range<usize>,
    row_count: usize,
    has_more_pages: bool,
    already_loading: bool,
) -> bool {
    if !has_more_pages || already_loading {
        return false;
    }
    visible_range.end + PREFETCH_MARGIN >= row_count
}

/// Parses `--open demo-bucket/photos/2026/` into a bucket and a prefix.
fn requested_location() -> Option<(String, String)> {
    let mut args = std::env::args();
    let value = loop {
        match args.next()?.as_str() {
            "--open" => break args.next()?,
            _ => continue,
        }
    };
    let (bucket, prefix) = match value.split_once('/') {
        Some((bucket, prefix)) => (bucket.to_string(), prefix.to_string()),
        None => (value, String::new()),
    };
    if bucket.is_empty() {
        return None;
    }
    // A prefix must end in `/` or the delimiter listing returns nothing.
    let prefix = if prefix.is_empty() || prefix.ends_with('/') {
        prefix
    } else {
        format!("{prefix}/")
    };
    Some((bucket, prefix))
}

/// How many clicks this gesture was. Keyboard-activated clicks count as one.
fn click_count(event: &ClickEvent) -> usize {
    match event {
        ClickEvent::Mouse(mouse) => mouse.up.click_count,
        ClickEvent::Keyboard(_) => 1,
    }
}

/// Cmd on macOS, Ctrl elsewhere.
fn is_primary(modifiers: &Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.platform
    } else {
        modifiers.control
    }
}

/// `a/b/c/` → `a/b/`, `a/` → ``, `` → ``.
fn parent_prefix(prefix: &str) -> String {
    prefix
        .trim_end_matches('/')
        .rsplit_once('/')
        .map(|(head, _)| format!("{head}/"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Mode;

    fn entry(name: &str, is_folder: bool, size: i64) -> Entry {
        Entry {
            name: name.into(),
            key: if is_folder {
                format!("{name}/")
            } else {
                name.into()
            },
            is_folder,
            size,
            modified_epoch: None,
            storage_class: None,
        }
    }

    /// A browser with entries but no connection, for testing view logic without
    /// a window or a network.
    fn offline(entries: Vec<Entry>, cx: &mut Context<Browser>) -> Browser {
        let mut browser = Browser {
            focus: cx.focus_handle(),
            theme: Theme::new(Mode::Dark, Chrome::Solid),
            chrome: Chrome::Solid,
            profiles: Vec::new(),
            active_profile: None,
            store: None,
            client: None,
            buckets: Vec::new(),
            bucket: Some("demo".into()),
            prefix: String::new(),
            entries,
            visible: Vec::new(),
            continuation: None,
            loading: false,
            loading_more: false,
            generation: 0,
            sort: Sort::default(),
            filter: String::new(),
            prompt: None,
            prompt_text: String::new(),
            selection: HashSet::new(),
            scroll: UniformListScrollHandle::new(),
            status: "test".into(),
            error: None,
            transfers: TransferEngine::in_memory().expect("in-memory queue"),
            drawer_open: false,
            ticking: false,
            connect_task: None,
            listing_task: None,
            paging_task: None,
            op_task: None,
            tick_task: None,
            _appearance: None,
        };
        browser.resort_and_filter();
        browser
    }

    #[test]
    fn parent_prefix_walks_up_one_level() {
        assert_eq!(parent_prefix("photos/2026/"), "photos/");
        assert_eq!(parent_prefix("photos/"), "");
        assert_eq!(parent_prefix(""), "");
        // Missing trailing slash should still behave.
        assert_eq!(parent_prefix("photos/2026"), "photos/");
    }

    #[gpui::test]
    fn filter_narrows_the_visible_rows_without_dropping_data(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            offline(
                vec![
                    entry("reports", true, 0),
                    entry("readme.txt", false, 21),
                    entry("blob.bin", false, 3_000_000),
                ],
                cx,
            )
        });

        entity.update(cx, |browser, _| {
            assert_eq!(browser.visible.len(), 3);

            browser.filter = "re".into();
            browser.resort_and_filter();
            let names: Vec<_> = browser
                .visible
                .iter()
                .map(|&ix| browser.entries[ix].name.as_str())
                .collect();
            assert_eq!(names, vec!["reports", "readme.txt"]);
            assert_eq!(browser.entries.len(), 3, "filtering must not discard data");

            browser.filter.clear();
            browser.resort_and_filter();
            assert_eq!(browser.visible.len(), 3, "clearing restores every row");
        });
    }

    #[gpui::test]
    fn sorting_keeps_folders_on_top_and_survives_filtering(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            offline(
                vec![
                    entry("archive.zip", false, 900),
                    entry("assets", true, 0),
                    entry("app.log", false, 10),
                ],
                cx,
            )
        });

        entity.update(cx, |browser, cx| {
            browser.toggle_sort(SortKey::Size, cx);
            let order: Vec<_> = browser
                .visible
                .iter()
                .map(|&ix| browser.entries[ix].name.as_str())
                .collect();
            assert_eq!(order, vec!["assets", "app.log", "archive.zip"]);

            // Same column again flips direction, folder still leads.
            browser.toggle_sort(SortKey::Size, cx);
            let order: Vec<_> = browser
                .visible
                .iter()
                .map(|&ix| browser.entries[ix].name.as_str())
                .collect();
            assert_eq!(order, vec!["assets", "archive.zip", "app.log"]);
        });
    }

    #[gpui::test]
    fn breadcrumbs_expand_each_prefix_level(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            let mut browser = offline(Vec::new(), cx);
            browser.prefix = "photos/2026/summer/".into();
            browser
        });

        entity.update(cx, |browser, _| {
            let crumbs = browser.breadcrumbs();
            let labels: Vec<_> = crumbs.iter().map(|(name, _)| name.to_string()).collect();
            let targets: Vec<_> = crumbs.iter().map(|(_, prefix)| prefix.as_str()).collect();
            assert_eq!(labels, vec!["photos", "2026", "summer"]);
            assert_eq!(
                targets,
                vec!["photos/", "photos/2026/", "photos/2026/summer/"],
                "each crumb navigates to its own level"
            );
        });
    }

    #[test]
    fn prefetch_fires_only_near_the_end_with_more_pages_and_no_request_in_flight() {
        let rows = 1000;

        // Scrolled near the end and the server said there is more.
        assert!(should_prefetch(&(940..1000), rows, true, false));
        // Exactly at the margin boundary.
        assert!(should_prefetch(&(900..960), rows, true, false));
        // One row short of it.
        assert!(!should_prefetch(&(899..959), rows, true, false));
        // Near the top.
        assert!(!should_prefetch(&(0..24), rows, true, false));
        // Last page already loaded: no continuation token, never fetch.
        assert!(!should_prefetch(&(940..1000), rows, false, false));
        // A request is already in flight; a second would duplicate rows.
        assert!(!should_prefetch(&(940..1000), rows, true, true));
        // A short listing that fits on screen still must not fetch when done.
        assert!(!should_prefetch(&(0..5), 5, false, false));
    }
}
