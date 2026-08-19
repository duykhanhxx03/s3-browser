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
use s3core::{
    format_size, format_timestamp, restore_state, sort_entries, Entry, ObjectHead,
    OrphanedUpload, Profile, RestoreState, S3Client, Sort, SortKey,
};
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
    /// Renaming the one selected entry; carries its key so the rename still
    /// targets the right object if the selection changes while typing.
    Rename(String),
    /// Adding a tag to the inspected object, typed as `khoá=giá trị`.
    AddTag,
}

impl Prompt {
    fn label(&self) -> &'static str {
        match self {
            Prompt::Filter => "Lọc",
            Prompt::NewFolder => "Tên thư mục mới",
            Prompt::NewBucket => "Tên bucket mới",
            Prompt::Rename(_) => "Tên mới",
            Prompt::AddTag => "Thẻ mới (khoá=giá trị)",
        }
    }
}

/// A destructive action waiting for the user to say yes. Holding the entries
/// rather than re-reading the selection means the dialog acts on exactly what
/// it described, even if the listing refreshes underneath it.
pub struct Confirm {
    title: SharedString,
    detail: SharedString,
    doomed: Vec<Entry>,
}

/// The share panel's state. Signing is a request, so the URL arrives after the
/// panel opens.
pub struct Share {
    key: String,
    url: Option<SharedString>,
    /// Set when the profile signs with a session token, which caps how long any
    /// URL can really last.
    temporary_credentials: bool,
}

/// The inspector's contents for one object. Metadata costs a HEAD and tags cost
/// another request, so this is only ever filled in when the panel is open — a
/// listing must never pay for it.
pub struct Inspection {
    key: String,
    head: Option<ObjectHead>,
    tags: Vec<(String, String)>,
    loading: bool,
    preview: Option<Preview>,
}

/// What the inspector can show of an object's contents. Only ever holds the
/// first `PREVIEW_LIMIT` bytes: a preview must never turn into an accidental
/// download of a multi-gigabyte object.
pub enum Preview {
    Image(std::sync::Arc<gpui::Image>),
    Text(SharedString),
    /// Fetched, but not something worth rendering as either.
    Unsupported,
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

    orphans: Vec<OrphanedUpload>,
    orphans_open: bool,

    confirm: Option<Confirm>,
    /// The share panel: which key it is for, and the URL once it exists.
    share: Option<Share>,
    inspector: Option<Inspection>,
    /// Whether the open bucket keeps versions, so a delete confirmation can say
    /// whether it removes data or only hides it. Refreshed when the bucket
    /// changes, not on every navigation within one.
    bucket_versioned: bool,
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

            orphans: Vec::new(),
            orphans_open: false,
            confirm: None,
            share: None,
            inspector: None,
            bucket_versioned: false,
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
            session_token,
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
            // An STS/SSO profile without its token authenticates nowhere.
            if let Err(error) = vault::set_session_token(&profile.id, session_token.as_deref()) {
                self.error = Some(format!("Không lưu được session token cho {}: {error}", profile.name).into());
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
            session_token: vault::session_token(&stored.id),
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

        // Only when the bucket actually changes: this costs a request, and it is
        // the same answer for every prefix inside one bucket.
        if self.bucket.as_ref() != Some(&bucket) {
            self.bucket_versioned = false;
            self.refresh_versioning(bucket.clone(), cx);
        }
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

    fn refresh_versioning(&mut self, bucket: SharedString, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let checking = Tokio::spawn(cx, async move { client.bucket_is_versioned(&bucket).await });
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let versioned = checking.await.unwrap_or(false);
            _ = this.update(cx, |this, cx| {
                this.bucket_versioned = versioned;
                cx.notify();
            });
        }));
    }

    /// Asks before deleting. Everything here is irreversible in a way the user
    /// cannot undo from the app, so the count and the consequence are spelled
    /// out rather than left to a generic "are you sure".
    fn ask_delete_selection(&mut self, cx: &mut Context<Self>) {
        let doomed: Vec<Entry> = self
            .entries
            .iter()
            .filter(|entry| self.selection.contains(&entry.key))
            .cloned()
            .collect();
        if doomed.is_empty() {
            return;
        }

        self.confirm = Some(Confirm {
            title: delete_title(&doomed).into(),
            detail: delete_detail(&doomed, self.bucket_versioned).into(),
            doomed,
        });
        cx.notify();
    }

    fn cancel_confirm(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        cx.notify();
    }

    fn commit_confirm(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        self.delete_entries(confirm.doomed, cx);
    }

    fn delete_entries(&mut self, doomed: Vec<Entry>, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
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

    /// Opens the inspector for the selected object and loads its details. This
    /// is the only place metadata is fetched: doing it while listing would mean
    /// a HEAD per row, which is the mistake that makes other S3 clients slow and
    /// expensive.
    fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        if self.inspector.is_some() {
            self.inspector = None;
            cx.notify();
            return;
        }
        self.open_inspector(cx);
    }

    fn open_inspector(&mut self, cx: &mut Context<Self>) {
        let mut selected = self.selection.iter();
        let (Some(key), None) = (selected.next(), selected.next()) else {
            return;
        };
        if key.ends_with('/') {
            self.report("Thư mục không có metadata".into());
            return;
        }
        let key = key.clone();
        self.inspector = Some(Inspection {
            key: key.clone(),
            head: None,
            tags: Vec::new(),
            loading: true,
            preview: None,
        });
        self.load_inspection(key, cx);
        cx.notify();
    }

    fn load_inspection(&mut self, key: String, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };

        let loading = Tokio::spawn(cx, async move {
            let head = client.head_object(&bucket, &key).await;
            // Tagging is a separate request, and a provider that does not
            // implement it must not blank out the metadata that did load.
            let tags = client.object_tags(&bucket, &key).await.unwrap_or_default();
            head.map(|head| (head, tags))
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = loading.await;
            _ = this.update(cx, |this, cx| {
                if let Some(inspector) = this.inspector.as_mut() {
                    inspector.loading = false;
                    match outcome {
                        Ok(Ok((head, tags))) => {
                            inspector.head = Some(head);
                            inspector.tags = tags;
                        }
                        Ok(Err(error)) => this.report(format!("{error}")),
                        Err(error) => this.report(format!("Task lỗi: {error}")),
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Fetches the first slice of the object and decides what it is. Runs only
    /// on demand: a preview of every selected row would be a download per click.
    fn load_preview(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(inspector)) = (
            self.client.clone(),
            self.bucket.clone(),
            self.inspector.as_ref(),
        ) else {
            return;
        };
        let key = inspector.key.clone();
        let key_for_format = key.clone();
        let size = inspector.head.as_ref().map(|head| head.size).unwrap_or(0);
        let kind = preview_kind(&key, inspector.head.as_ref().and_then(|h| h.content_type.as_deref()));

        if kind == PreviewKind::None {
            if let Some(inspector) = self.inspector.as_mut() {
                inspector.preview = Some(Preview::Unsupported);
            }
            cx.notify();
            return;
        }

        // An image has to arrive whole to decode, so an oversized one is refused
        // rather than fetched and shown broken. Text is fine truncated.
        if kind == PreviewKind::Image && size > PREVIEW_LIMIT as i64 {
            if let Some(inspector) = self.inspector.as_mut() {
                inspector.preview = Some(Preview::Unsupported);
            }
            self.status = "Ảnh quá lớn để xem trước".into();
            cx.notify();
            return;
        }

        let wanted = (size.max(0) as u64).min(PREVIEW_LIMIT);
        if wanted == 0 {
            if let Some(inspector) = self.inspector.as_mut() {
                inspector.preview = Some(Preview::Unsupported);
            }
            cx.notify();
            return;
        }

        let fetching = Tokio::spawn(cx, async move {
            client.get_range(&bucket, &key, 0..wanted, None).await
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = fetching.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(bytes)) => {
                        if let Some(inspector) = this.inspector.as_mut() {
                            inspector.preview = Some(build_preview(kind, &key_for_format, bytes));
                        }
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
    }

    /// Downloads the object to a temporary file and hands it to whatever the OS
    /// opens that file type with.
    fn open_externally(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(inspector)) = (
            self.client.clone(),
            self.bucket.clone(),
            self.inspector.as_ref(),
        ) else {
            return;
        };
        let key = inspector.key.clone();
        let size = inspector.head.as_ref().map(|head| head.size).unwrap_or(0).max(0) as u64;
        let name = entry_name_of(&key);

        self.status = format!("Đang tải {name} để mở…").into();

        let fetching = Tokio::spawn(cx, async move {
            let bytes = client.get_range(&bucket, &key, 0..size, None).await?;
            // A per-run subdirectory keeps two objects with the same name from
            // overwriting each other.
            let dir = std::env::temp_dir().join(format!("s3browser-{}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(&name);
            std::fs::write(&path, bytes)?;
            anyhow::Ok(path)
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = fetching.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(path)) => match opener::open(&path) {
                        Ok(()) => this.status = format!("Đã mở {}", path.display()).into(),
                        Err(error) => this.report(format!("Không mở được: {error}")),
                    },
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
    }

    /// Space opens the inspector on the selection and previews it in one go —
    /// the Finder gesture, which is what people reach for.
    fn quick_look(&mut self, cx: &mut Context<Self>) {
        if self.inspector.is_none() {
            self.open_inspector(cx);
        }
        if self.inspector.is_some() {
            self.load_preview(cx);
        }
    }

    fn restore_inspected(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(inspector)) = (
            self.client.clone(),
            self.bucket.clone(),
            self.inspector.as_ref(),
        ) else {
            return;
        };
        let key = inspector.key.clone();
        let reload = key.clone();

        let restoring = Tokio::spawn(cx, async move {
            // Three days is enough to fetch something without paying for a copy
            // that lingers.
            client.restore_object(&bucket, &key, 3).await
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = restoring.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(())) => {
                        this.status = "Đã yêu cầu khôi phục — có thể mất vài giờ".into();
                        this.load_inspection(reload, cx);
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
    }

    fn remove_tag(&mut self, tag_key: String, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(inspector)) = (
            self.client.clone(),
            self.bucket.clone(),
            self.inspector.as_ref(),
        ) else {
            return;
        };
        let key = inspector.key.clone();
        // S3 replaces the whole tag set, so removing one means writing the rest.
        let remaining: Vec<(String, String)> = inspector
            .tags
            .iter()
            .filter(|(name, _)| name != &tag_key)
            .cloned()
            .collect();
        let reload = key.clone();

        let writing = Tokio::spawn(cx, async move {
            client.set_object_tags(&bucket, &key, &remaining).await
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = writing.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(())) => this.load_inspection(reload, cx),
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
    }

    fn add_tag(&mut self, text: String, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(inspector)) = (
            self.client.clone(),
            self.bucket.clone(),
            self.inspector.as_ref(),
        ) else {
            return;
        };
        let Some((name, value)) = parse_tag(&text) else {
            self.report("Thẻ phải có dạng khoá=giá trị".into());
            return;
        };

        let key = inspector.key.clone();
        let mut tags = inspector.tags.clone();
        // Same key twice is a rejected tag set, so a repeat is an edit.
        tags.retain(|(existing, _)| existing != &name);
        tags.push((name, value));
        let reload = key.clone();

        let writing = Tokio::spawn(cx, async move {
            client.set_object_tags(&bucket, &key, &tags).await
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = writing.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(())) => this.load_inspection(reload, cx),
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
    }

    /// Opens the share panel for the one selected object. Folders are excluded:
    /// a prefix is not a thing that can be signed.
    fn start_share(&mut self, cx: &mut Context<Self>) {
        let mut selected = self.selection.iter();
        let (Some(key), None) = (selected.next(), selected.next()) else {
            return;
        };
        if key.ends_with('/') {
            self.report("Không tạo được link cho thư mục".into());
            return;
        }

        let temporary_credentials = self
            .active_profile
            .and_then(|ix| self.profiles.get(ix))
            .is_some_and(|stored| vault::session_token(&stored.id).is_some());

        self.share = Some(Share {
            key: key.clone(),
            url: None,
            temporary_credentials,
        });
        self.presign(PRESIGN_PRESETS[0].1, cx);
        cx.notify();
    }

    fn presign(&mut self, expires: Duration, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(share)) =
            (self.client.clone(), self.bucket.clone(), self.share.as_ref())
        else {
            return;
        };
        // Never sign for longer than the credentials can actually honour.
        let capped = expires.min(s3core::presign_limit_for(share.temporary_credentials));
        let key = share.key.clone();

        let signing = Tokio::spawn(cx, async move {
            client.presign_get(&bucket, &key, capped).await
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = signing.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(url)) => {
                        if let Some(share) = this.share.as_mut() {
                            share.url = Some(url.into());
                        }
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
    }

    fn copy_to_clipboard(&mut self, text: String, what: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.status = format!("Đã chép {what}").into();
        cx.notify();
    }

    /// The unsigned URL. Only useful when the object is public, which the app
    /// cannot know without reading the bucket policy — so the panel says so
    /// rather than implying the link works for anyone.
    fn copy_public_url(&mut self, cx: &mut Context<Self>) {
        let (Some(bucket), Some(share)) = (self.bucket.clone(), self.share.as_ref()) else {
            return;
        };
        let Some(profile) = self
            .active_profile
            .and_then(|ix| self.profiles.get(ix))
            .cloned()
        else {
            return;
        };

        let profile = Profile {
            name: profile.name,
            endpoint: profile.endpoint,
            region: profile.region,
            path_style: profile.path_style,
            access_key: profile.access_key,
            // Only the addressing style matters here; nothing is signed.
            secret_key: String::new(),
            session_token: None,
            relaxed_checksums: profile.relaxed_checksums,
        };
        let url = s3core::public_url(&profile, &bucket, &share.key);
        self.copy_to_clipboard(url, "URL công khai", cx);
    }

    /// Opens the rename prompt, but only for a single entry: renaming several
    /// things at once has no sensible meaning with one text field.
    fn start_rename(&mut self, cx: &mut Context<Self>) {
        let mut selected = self.selection.iter();
        let (Some(key), None) = (selected.next(), selected.next()) else {
            return;
        };
        let key = key.clone();
        self.start_prompt(Prompt::Rename(key), cx);
    }

    /// Renames one entry. A folder is a prefix, so renaming it moves every key
    /// underneath; a single object is one copy plus one delete.
    fn rename_entry(&mut self, key: String, new_name: String, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        let Some(target) = renamed_key(&key, &new_name) else {
            self.report("Tên mới không hợp lệ".into());
            return;
        };
        if target == key {
            return;
        }

        let is_folder = key.ends_with('/');
        let bucket_name = bucket.to_string();
        let source = key.clone();
        let destination = target.clone();

        let renaming = Tokio::spawn(cx, async move {
            if is_folder {
                client
                    .move_prefix(&bucket_name, &source, &destination, |_, _| {})
                    .await
                    .map(|report| report.errors)
            } else {
                client
                    .move_object(&bucket_name, &source, &bucket_name, &destination)
                    .await
                    .map(|()| Vec::new())
            }
        });

        self.status = format!("Đang đổi tên {}…", entry_name_of(&key)).into();
        let task = cx.spawn(async move |this, cx| {
            let outcome = renaming.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(errors)) if errors.is_empty() => {
                        this.status = format!("Đã đổi tên thành {new_name}").into();
                    }
                    // A partial move leaves keys on both sides; saying "done"
                    // would send the user away from the ones still stuck.
                    Ok(Ok(errors)) => this.report(format!(
                        "Đổi tên chưa xong: {} mục lỗi. {}",
                        errors.len(),
                        errors.first().cloned().unwrap_or_default()
                    )),
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                this.selection.clear();
                if let (Some(bucket), prefix) = (this.bucket.clone(), this.prefix.clone()) {
                    this.open(bucket, prefix, cx);
                }
                cx.notify();
            });
        });
        self.op_task = Some(task);
        cx.notify();
    }

    /// Looks for multipart uploads left behind by a crash or a cancel. S3 bills
    /// for their parts indefinitely and they never show up in a normal listing,
    /// so this is the only way to find them.
    fn scan_orphans(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        self.orphans_open = true;
        self.status = "Đang tìm upload dở…".into();

        let scanning = Tokio::spawn(cx, async move { client.list_orphaned_uploads(&bucket).await });

        let task = cx.spawn(async move |this, cx| {
            let outcome = scanning.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(orphans)) => {
                        this.status = if orphans.is_empty() {
                            "Không có upload dở nào".into()
                        } else {
                            format!("Tìm thấy {} upload dở", orphans.len()).into()
                        };
                        this.orphans = orphans;
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

    /// Aborts one orphaned upload, releasing the storage it was holding.
    fn abort_orphan(&mut self, upload_id: SharedString, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        let Some(orphan) = self
            .orphans
            .iter()
            .find(|orphan| orphan.upload_id == upload_id.as_ref())
            .cloned()
        else {
            return;
        };

        let aborting = Tokio::spawn(cx, async move {
            client
                .abort_multipart_upload(&bucket, &orphan.key, &orphan.upload_id)
                .await
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = aborting.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(())) => this.orphans.retain(|o| o.upload_id != upload_id.as_ref()),
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        });
        self.op_task = Some(task);
        cx.notify();
    }

    /// Aborts every orphan currently listed, reporting how many failed rather
    /// than stopping at the first error.
    fn abort_all_orphans(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        let orphans = self.orphans.clone();
        if orphans.is_empty() {
            return;
        }

        let aborting = Tokio::spawn(cx, async move {
            let mut failures = 0usize;
            for orphan in &orphans {
                if client
                    .abort_multipart_upload(&bucket, &orphan.key, &orphan.upload_id)
                    .await
                    .is_err()
                {
                    failures += 1;
                }
            }
            (orphans.len(), failures)
        });

        let task = cx.spawn(async move |this, cx| {
            let (total, failures) = aborting.await.unwrap_or((0, 0));
            _ = this.update(cx, |this, cx| {
                // Anything that failed is still out there, so re-scan rather than
                // leaving the list claiming the bucket is clean.
                this.orphans.clear();
                this.status = abort_summary(total, failures).into();
                if failures > 0 {
                    this.scan_orphans(cx);
                }
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
        self.prompt_text = match &prompt {
            Prompt::Filter => self.filter.clone(),
            // Start from the existing name so a rename is an edit, not retyping.
            Prompt::Rename(key) => entry_name_of(key),
            _ => String::new(),
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
            Prompt::Rename(key) if !text.trim().is_empty() => {
                self.rename_entry(key, text.trim().to_string(), cx)
            }
            Prompt::AddTag if !text.trim().is_empty() => self.add_tag(text, cx),
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

        // A confirmation takes the keyboard entirely: ⌘⌫ while it is up must not
        // queue up a second delete behind the one being asked about.
        if self.confirm.is_some() {
            match keystroke.key.as_str() {
                "escape" => return self.cancel_confirm(cx),
                "enter" => return self.commit_confirm(cx),
                _ => return,
            }
        }

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
                "enter" => return self.start_rename(cx),
                "i" => return self.toggle_inspector(cx),
                "j" => {
                    self.drawer_open = !self.drawer_open;
                    cx.notify();
                    return;
                }
                "backspace" | "delete" => return self.ask_delete_selection(cx),
                "up" => return self.go_up(cx),
                _ => {}
            }
        }

        if self.prompt.is_none() && keystroke.key == "space" && !self.selection.is_empty() {
            return self.quick_look(cx);
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
            .when(bucket.is_some(), |this| {
                this.child(
                    action_button("scan-orphans", "Dọn upload dở", theme).on_click(cx.listener(
                        |this, _event, _window, cx| this.scan_orphans(cx),
                    )),
                )
            })
            .when(self.selection.len() == 1, |this| {
                this.child(
                    action_button("rename", "Đổi tên", theme).on_click(cx.listener(
                        |this, _event, _window, cx| this.start_rename(cx),
                    )),
                )
                .child(
                    action_button("share", "Chia sẻ", theme).on_click(cx.listener(
                        |this, _event, _window, cx| this.start_share(cx),
                    )),
                )
                .child(
                    action_button("inspect", "Chi tiết", theme).on_click(cx.listener(
                        |this, _event, _window, cx| this.toggle_inspector(cx),
                    )),
                )
            })
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
                            this.ask_delete_selection(cx)
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
                            action_button("bandwidth", "", theme)
                                .child(SharedString::from(format!(
                                    "Băng thông: {}",
                                    bandwidth_label(self.transfers.bandwidth_limit())
                                )))
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    let next = next_bandwidth_limit(
                                        this.transfers.bandwidth_limit(),
                                    );
                                    this.transfers.set_bandwidth_limit(next);
                                    cx.notify();
                                })),
                        )
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

    fn render_inspector(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let inspector = self.inspector.as_ref()?;
        let theme = self.theme;

        let body = match (&inspector.head, inspector.loading) {
            (None, true) => div()
                .p_3()
                .text_xs()
                .text_color(theme.text_faint)
                .child("Đang tải…")
                .into_any_element(),
            (None, false) => div()
                .p_3()
                .text_xs()
                .text_color(theme.text_faint)
                .child("Không đọc được metadata")
                .into_any_element(),
            (Some(head), _) => {
                let class = head.storage_class.as_deref().unwrap_or("STANDARD");
                let state = restore_state(head.restore.as_deref(), head.storage_class.as_deref());

                div()
                    .id("inspector-body")
                    .flex_1()
                    .overflow_hidden()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .text_xs()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(detail_row("Kích thước", format_size(head.size), theme))
                            .child(detail_row(
                                "Sửa đổi",
                                head.modified_epoch.map(format_timestamp).unwrap_or_default(),
                                theme,
                            ))
                            .child(detail_row(
                                "Kiểu",
                                head.content_type.clone().unwrap_or_default(),
                                theme,
                            ))
                            .child(detail_row("Lớp lưu trữ", class.to_string(), theme))
                            .child(detail_row(
                                "ETag",
                                head.etag.clone().unwrap_or_default().replace('"', ""),
                                theme,
                            )),
                    )
                    // Only archived objects get the restore control, and its
                    // label says which of the three states it is in.
                    .when(state != RestoreState::NotArchived, |this| {
                        this.child(match state {
                            RestoreState::Archived => {
                                action_button("restore", "Khôi phục (3 ngày)", theme)
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.restore_inspected(cx)
                                    }))
                                    .into_any_element()
                            }
                            RestoreState::InProgress => div()
                                .text_color(theme.text_muted)
                                .child("Đang khôi phục — chưa đọc được")
                                .into_any_element(),
                            _ => div()
                                .text_color(theme.text_muted)
                                .child("Đã khôi phục, đọc được tạm thời")
                                .into_any_element(),
                        })
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_color(theme.text_faint)
                                            .child("THẺ"),
                                    )
                                    .child(action_button("add-tag", "Thêm", theme).on_click(
                                        cx.listener(|this, _event, _window, cx| {
                                            this.start_prompt(Prompt::AddTag, cx)
                                        }),
                                    )),
                            )
                            .children(inspector.tags.iter().map(|(name, value)| {
                                let name = name.clone();
                                let removing = name.clone();
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_color(theme.text)
                                            .child(SharedString::from(format!("{name} = {value}"))),
                                    )
                                    .child(
                                        icon_button_dyn(
                                            SharedString::from(format!("rm-tag-{name}")),
                                            "✕",
                                            theme,
                                        )
                                        .on_click(cx.listener(
                                            move |this, _event, _window, cx| {
                                                this.remove_tag(removing.clone(), cx)
                                            },
                                        )),
                                    )
                            }))
                            .when(inspector.tags.is_empty(), |this| {
                                this.child(
                                    div().text_color(theme.text_faint).child("Chưa có thẻ nào"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                action_button("preview", "Xem trước", theme).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.load_preview(cx)
                                    }),
                                ),
                            )
                            .child(
                                action_button("open-external", "Mở bằng app", theme).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.open_externally(cx)
                                    }),
                                ),
                            ),
                    )
                    .children(inspector.preview.as_ref().map(|preview| match preview {
                        Preview::Image(image) => gpui::img(image.clone())
                            .max_w_full()
                            .max_h(px(220.))
                            .into_any_element(),
                        Preview::Text(text) => div()
                            .id("preview-text")
                            .max_h(px(220.))
                            .overflow_hidden()
                            .p_2()
                            .rounded_md()
                            .bg(theme.hover)
                            .font_family("monospace")
                            .text_color(theme.text_muted)
                            .child(text.clone())
                            .into_any_element(),
                        Preview::Unsupported => div()
                            .text_color(theme.text_faint)
                            .child("Không xem trước được kiểu này")
                            .into_any_element(),
                    }))
                    .when(!head.metadata.is_empty(), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(div().text_color(theme.text_faint).child("METADATA"))
                                .children(head.metadata.iter().map(|(name, value)| {
                                    div().text_color(theme.text).child(SharedString::from(
                                        format!("{name} = {value}"),
                                    ))
                                })),
                        )
                    })
                    .into_any_element()
            }
        };

        Some(
            div()
                .w(px(300.))
                .h_full()
                .flex()
                .flex_col()
                .bg(theme.panel)
                .border_l_1()
                .border_color(theme.border)
                .child(
                    div()
                        .h(px(28.))
                        .px_3()
                        .flex()
                        .items_center()
                        .gap_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(theme.text)
                                .overflow_hidden()
                                .child(SharedString::from(entry_name_of(&inspector.key))),
                        )
                        .child(icon_button("close-inspector", "✕", theme).on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.inspector = None;
                                cx.notify();
                            },
                        ))),
                )
                .child(body),
        )
    }

    fn render_share(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let share = self.share.as_ref()?;
        let theme = self.theme;
        let name = entry_name_of(&share.key);

        Some(
            div()
                .id("share-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.45))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.share = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("share-dialog")
                        .w(px(460.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_lg()
                        .bg(theme.panel)
                        .border_1()
                        .border_color(theme.border_strong)
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(
                            div()
                                .text_color(theme.text)
                                .child(SharedString::from(format!("Chia sẻ {name}"))),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .children(PRESIGN_PRESETS.iter().map(|(label, expires)| {
                                    let expires = *expires;
                                    action_button_dyn(
                                        SharedString::from(format!("presign-{label}")),
                                        SharedString::from(*label),
                                        theme,
                                    )
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        this.presign(expires, cx)
                                    }))
                                })),
                        )
                        // The credential warning is the whole point of the panel:
                        // a link that dies with the session looks identical to one
                        // that lasts a week.
                        .when(share.temporary_credentials, |this| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.danger)
                                    .child(
                                        "Profile này dùng credential tạm thời (STS/SSO): link sẽ chết khi session hết hạn, dù chọn thời hạn dài hơn.",
                                    ),
                            )
                        })
                        .child(match &share.url {
                            Some(url) => div()
                                .id("share-url")
                                .p_2()
                                .rounded_md()
                                .bg(theme.hover)
                                .text_xs()
                                .text_color(theme.text_muted)
                                .child(url.clone())
                                .into_any_element(),
                            None => div()
                                .text_xs()
                                .text_color(theme.text_faint)
                                .child("Đang ký…")
                                .into_any_element(),
                        })
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    action_button("copy-public", "Chép URL công khai", theme)
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.copy_public_url(cx)
                                        })),
                                )
                                .when_some(share.url.clone(), |this, url| {
                                    this.child(
                                        action_button("copy-signed", "Chép link", theme).on_click(
                                            cx.listener(move |this, _event, _window, cx| {
                                                this.copy_to_clipboard(
                                                    url.to_string(),
                                                    "link",
                                                    cx,
                                                )
                                            }),
                                        ),
                                    )
                                })
                                .child(action_button("share-close", "Đóng", theme).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.share = None;
                                        cx.notify();
                                    }),
                                )),
                        ),
                ),
        )
    }

    fn render_confirm(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let confirm = self.confirm.as_ref()?;
        let theme = self.theme;

        Some(
            // A full-bleed scrim: it dims the list and, more importantly, means
            // a stray click lands here instead of on the thing behind it.
            div()
                .id("confirm-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.45))
                .on_click(cx.listener(|this, _event, _window, cx| this.cancel_confirm(cx)))
                .child(
                    div()
                        .id("confirm-dialog")
                        .w(px(380.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_lg()
                        .bg(theme.panel)
                        .border_1()
                        .border_color(theme.border_strong)
                        // Clicks inside the dialog must not reach the scrim.
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(
                            div()
                                .text_color(theme.text)
                                .child(confirm.title.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.text_muted)
                                .child(confirm.detail.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .justify_end()
                                .child(action_button("confirm-cancel", "Huỷ", theme).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.cancel_confirm(cx)
                                    }),
                                ))
                                .child(
                                    danger_button("confirm-ok", "Xoá".into(), theme).on_click(
                                        cx.listener(|this, _event, _window, cx| {
                                            this.commit_confirm(cx)
                                        }),
                                    ),
                                ),
                        ),
                ),
        )
    }

    fn render_orphans(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.orphans_open {
            return None;
        }
        let theme = self.theme;
        let count = self.orphans.len();

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
                        .child(SharedString::from(format!("UPLOAD DỞ · {count}")))
                        .child(div().flex_1())
                        .when(count > 0, |this| {
                            this.child(danger_button(
                                "abort-all-orphans",
                                SharedString::from(format!("Huỷ tất cả ({count})")),
                                theme,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.abort_all_orphans(cx)
                            })))
                        })
                        .child(
                            action_button("close-orphans", "Đóng", theme).on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.orphans_open = false;
                                    cx.notify();
                                },
                            )),
                        ),
                )
                .child(if self.orphans.is_empty() {
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(theme.text_faint)
                        .child("Không có upload dở nào")
                        .into_any_element()
                } else {
                    uniform_list(
                        "orphans",
                        self.orphans.len(),
                        cx.processor(move |this, range: Range<usize>, _window, cx| {
                            range
                                .filter_map(|ix| this.orphans.get(ix).cloned())
                                .map(|orphan| this.render_orphan(orphan, cx))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .flex_1()
                    .into_any_element()
                }),
        )
    }

    fn render_orphan(&self, orphan: OrphanedUpload, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let upload_id = SharedString::from(orphan.upload_id.clone());
        let when: SharedString = orphan
            .initiated_epoch
            .map(format_timestamp)
            .unwrap_or_else(|| "?".into())
            .into();

        div()
            .id(upload_id.clone())
            .h(px(32.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .text_xs()
            .child(
                div()
                    .flex_1()
                    .text_color(theme.text)
                    .overflow_hidden()
                    .child(SharedString::from(orphan.key)),
            )
            .child(div().text_color(theme.text_faint).child(when))
            .child(action_button("abort-orphan", "Huỷ", theme).on_click(cx.listener(
                move |this, _event, _window, cx| this.abort_orphan(upload_id.clone(), cx),
            )))
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
                    )
                    .children(self.render_inspector(cx)),
            )
            .children(self.render_drawer(cx))
            .children(self.render_orphans(cx))
            .child(self.render_status(cx))
            .children(self.render_confirm(cx))
            .children(self.render_share(cx))
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

/// One `label: value` line in the inspector.
fn detail_row(label: &'static str, value: String, theme: Theme) -> impl IntoElement {
    div()
        .flex()
        .gap_2()
        .child(div().w(px(84.)).text_color(theme.text_faint).child(label))
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .text_color(theme.text)
                .child(SharedString::from(value)),
        )
}

/// `icon_button` for ids built from data.
fn icon_button_dyn(
    id: SharedString,
    glyph: &'static str,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(18.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_xs()
        .text_color(theme.text_muted)
        .hover(|style| style.bg(theme.hover))
        .child(glyph)
}

/// `action_button` for labels that come from data rather than a literal.
fn action_button_dyn(
    id: SharedString,
    label: SharedString,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_0p5()
        .rounded_md()
        .text_xs()
        .text_color(theme.text)
        .bg(theme.hover)
        .hover(|style| style.bg(theme.selected))
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

/// Bandwidth presets the drawer button cycles through, in bytes per second.
/// Zero is unlimited and comes last, so one more click always gets back to it.
const BANDWIDTH_PRESETS: [u64; 4] = [1_000_000, 5_000_000, 20_000_000, 0];

fn bandwidth_label(limit: u64) -> String {
    if limit == 0 {
        "không giới hạn".into()
    } else {
        format!("{} MB/s", limit / 1_000_000)
    }
}

/// The next preset after `current`. An unrecognised limit (set by something
/// other than this button) falls through to the first preset.
fn next_bandwidth_limit(current: u64) -> u64 {
    let at = BANDWIDTH_PRESETS.iter().position(|&preset| preset == current);
    match at {
        Some(ix) => BANDWIDTH_PRESETS[(ix + 1) % BANDWIDTH_PRESETS.len()],
        None => BANDWIDTH_PRESETS[0],
    }
}

/// Expiry choices for a presigned URL. Capped per-credential-type at sign time,
/// so a temporary-credential profile silently gets the shorter of the two.
const PRESIGN_PRESETS: [(&str, Duration); 4] = [
    ("1 giờ", Duration::from_secs(3600)),
    ("24 giờ", Duration::from_secs(24 * 3600)),
    ("7 ngày", Duration::from_secs(7 * 24 * 3600)),
    ("15 phút", Duration::from_secs(900)),
];

/// Turns fetched bytes into something renderable. Text that is not valid UTF-8
/// is treated as unsupported rather than shown as replacement characters —
/// mojibake looks like corruption of the object itself.
fn build_preview(kind: PreviewKind, key: &str, bytes: Vec<u8>) -> Preview {
    match kind {
        PreviewKind::Image => match image_format_for_key(key) {
            Some(format) => {
                Preview::Image(std::sync::Arc::new(gpui::Image::from_bytes(format, bytes)))
            }
            None => Preview::Unsupported,
        },
        PreviewKind::Text => match String::from_utf8(bytes) {
            Ok(text) => Preview::Text(text.into()),
            Err(_) => Preview::Unsupported,
        },
        PreviewKind::None => Preview::Unsupported,
    }
}

/// How much of an object a preview is allowed to fetch. Big enough for a photo,
/// small enough that previewing a huge object is never a surprise download.
const PREVIEW_LIMIT: u64 = 8 * 1024 * 1024;

/// What a preview should try to render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewKind {
    Image,
    Text,
    None,
}

/// Decides from the content type first and the extension second. The type is
/// what the object claims to be; the extension is a fallback for the many
/// objects uploaded as `application/octet-stream`.
fn preview_kind(key: &str, content_type: Option<&str>) -> PreviewKind {
    if let Some(mime) = content_type {
        let mime = mime.split(';').next().unwrap_or(mime).trim();
        if image_format_for_mime(mime).is_some() {
            return PreviewKind::Image;
        }
        // Structured text is still text: JSON and XML are worth reading inline.
        if mime.starts_with("text/")
            || matches!(mime, "application/json" | "application/xml" | "application/yaml")
        {
            return PreviewKind::Text;
        }
        // A specific, non-text type is a real answer — don't let the extension
        // override it.
        if mime != "application/octet-stream" && mime != "binary/octet-stream" {
            return PreviewKind::None;
        }
    }

    let extension = key.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => PreviewKind::Image,
        "txt" | "md" | "json" | "xml" | "yaml" | "yml" | "toml" | "csv" | "log" | "rs" | "py"
        | "js" | "ts" | "html" | "css" | "sh" | "sql" => PreviewKind::Text,
        _ => PreviewKind::None,
    }
}

/// Maps a MIME type to the format gpui needs to decode the bytes.
fn image_format_for_mime(mime: &str) -> Option<gpui::ImageFormat> {
    Some(match mime {
        "image/png" => gpui::ImageFormat::Png,
        "image/jpeg" | "image/jpg" => gpui::ImageFormat::Jpeg,
        "image/gif" => gpui::ImageFormat::Gif,
        "image/webp" => gpui::ImageFormat::Webp,
        "image/bmp" => gpui::ImageFormat::Bmp,
        "image/svg+xml" => gpui::ImageFormat::Svg,
        _ => return None,
    })
}

/// The format implied by a filename, for objects whose content type is unhelpful.
fn image_format_for_key(key: &str) -> Option<gpui::ImageFormat> {
    match key.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => Some(gpui::ImageFormat::Png),
        "jpg" | "jpeg" => Some(gpui::ImageFormat::Jpeg),
        "gif" => Some(gpui::ImageFormat::Gif),
        "webp" => Some(gpui::ImageFormat::Webp),
        "bmp" => Some(gpui::ImageFormat::Bmp),
        "svg" => Some(gpui::ImageFormat::Svg),
        _ => None,
    }
}

/// Splits `khoá=giá trị`. Only the first `=` separates, because an S3 tag value
/// may legitimately contain one. Returns `None` for input that would produce an
/// empty key, which S3 rejects anyway.
fn parse_tag(text: &str) -> Option<(String, String)> {
    let (name, value) = text.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), value.trim().to_string()))
}

/// Headline for the delete dialog. Names the single victim when there is one,
/// because "Xoá 1 mục" tells the user nothing about what they are about to lose.
fn delete_title(doomed: &[Entry]) -> String {
    match doomed {
        [only] => format!("Xoá {}?", entry_name_of(&only.key)),
        many => format!("Xoá {} mục?", many.len()),
    }
}

/// The consequence, spelled out. Folders are called out separately because
/// deleting one takes everything inside it — a listing showing three rows can
/// stand for thousands of keys.
fn delete_detail(doomed: &[Entry], versioned: bool) -> String {
    let folders = doomed.iter().filter(|entry| entry.is_folder).count();

    let mut detail = if folders > 0 {
        let noun = if folders == 1 { "thư mục" } else { "thư mục" };
        format!("Gồm {folders} {noun} — mọi thứ bên trong cũng bị xoá. ")
    } else {
        String::new()
    };

    detail.push_str(if versioned {
        // A delete marker is not a deletion; saying "permanent" here would be a
        // lie, and saying nothing would leave the user thinking data is gone.
        "Bucket này bật versioning: thao tác tạo delete marker, bản cũ vẫn còn và vẫn tính tiền lưu trữ."
    } else {
        "Không hoàn tác được."
    });
    detail
}

/// The display name of a key: its last path segment. A folder key ends in `/`,
/// which is part of the key but not part of its name.
fn entry_name_of(key: &str) -> String {
    key.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(key)
        .to_string()
}

/// The key an entry gets after renaming its last segment. Returns `None` for a
/// name that cannot be used — empty, or containing a slash, which would move the
/// entry somewhere else instead of renaming it.
///
/// A folder keeps its trailing slash, because losing it turns a prefix into a
/// zero-byte object with the folder's name.
fn renamed_key(key: &str, new_name: &str) -> Option<String> {
    let name = new_name.trim();
    if name.is_empty() || name.contains('/') {
        return None;
    }

    let is_folder = key.ends_with('/');
    let body = key.trim_end_matches('/');
    let parent = match body.rfind('/') {
        Some(ix) => &body[..=ix],
        None => "",
    };
    Some(format!(
        "{parent}{name}{}",
        if is_folder { "/" } else { "" }
    ))
}

/// What to tell the user after a bulk abort. Pure because the partial-failure
/// arithmetic is the easy thing to get backwards.
fn abort_summary(total: usize, failures: usize) -> String {
    let succeeded = total.saturating_sub(failures);
    if failures == 0 {
        format!("Đã huỷ {total} upload dở")
    } else if succeeded == 0 {
        format!("Không huỷ được upload nào ({failures} lỗi)")
    } else {
        format!("Đã huỷ {succeeded}/{total} upload dở, {failures} lỗi")
    }
}

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

            orphans: Vec::new(),
            orphans_open: false,
            confirm: None,
            share: None,
            inspector: None,
            bucket_versioned: false,
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

    #[test]
    fn bandwidth_presets_cycle_and_always_return_to_unlimited() {
        assert_eq!(bandwidth_label(0), "không giới hạn");
        assert_eq!(bandwidth_label(5_000_000), "5 MB/s");

        // Starting from unlimited, cycling through every preset comes back.
        let mut limit = 0;
        let mut seen = vec![limit];
        for _ in 0..BANDWIDTH_PRESETS.len() {
            limit = next_bandwidth_limit(limit);
            seen.push(limit);
        }
        assert_eq!(seen, vec![0, 1_000_000, 5_000_000, 20_000_000, 0]);

        // A limit this button never sets must not strand the user.
        assert_eq!(next_bandwidth_limit(999), BANDWIDTH_PRESETS[0]);
    }

    #[test]
    fn preview_kind_trusts_the_content_type_over_the_extension() {
        // A declared type wins: an image served as .dat is still an image.
        assert_eq!(preview_kind("blob.dat", Some("image/png")), PreviewKind::Image);
        assert_eq!(preview_kind("notes.png", Some("text/plain")), PreviewKind::Text);

        // Charset parameters must not defeat the match.
        assert_eq!(
            preview_kind("a.bin", Some("text/plain; charset=utf-8")),
            PreviewKind::Text
        );

        // Structured text is worth reading inline.
        assert_eq!(preview_kind("a.bin", Some("application/json")), PreviewKind::Text);

        // A specific non-text type is a real answer — the extension must not
        // override it into something we would then fail to render.
        assert_eq!(preview_kind("report.txt", Some("application/pdf")), PreviewKind::None);

        // octet-stream says nothing, so fall through to the extension. This is
        // the common case: most uploads carry no useful type at all.
        assert_eq!(
            preview_kind("photo.JPEG", Some("application/octet-stream")),
            PreviewKind::Image
        );
        assert_eq!(preview_kind("readme.md", None), PreviewKind::Text);
        assert_eq!(preview_kind("archive.zip", None), PreviewKind::None);
        assert_eq!(preview_kind("no-extension", None), PreviewKind::None);
    }

    #[test]
    fn undecodable_text_is_not_shown_as_mojibake() {
        // Invalid UTF-8 rendered with replacement characters looks like the
        // object itself is corrupt, which is a worse lie than declining.
        let preview = build_preview(PreviewKind::Text, "a.txt", vec![0xff, 0xfe, 0x00]);
        assert!(matches!(preview, Preview::Unsupported));

        let preview = build_preview(PreviewKind::Text, "a.txt", "xin chào".as_bytes().to_vec());
        match preview {
            Preview::Text(text) => assert_eq!(text.to_string(), "xin chào"),
            _ => panic!("valid UTF-8 should preview as text"),
        }

        // An image whose extension gives no format cannot be decoded.
        assert!(matches!(
            build_preview(PreviewKind::Image, "mystery", vec![1, 2, 3]),
            Preview::Unsupported
        ));
    }

    #[test]
    fn tags_split_on_the_first_equals_only() {
        assert_eq!(
            parse_tag("env=prod"),
            Some(("env".into(), "prod".into()))
        );

        // A value may contain `=`; splitting on the last one would mangle it.
        assert_eq!(
            parse_tag("url=https://x/?a=b"),
            Some(("url".into(), "https://x/?a=b".into()))
        );

        // Whitespace around either half is the user's formatting, not the tag.
        assert_eq!(
            parse_tag("  owner = mai  "),
            Some(("owner".into(), "mai".into()))
        );

        // An empty value is legal in S3; an empty key is not.
        assert_eq!(parse_tag("draft="), Some(("draft".into(), String::new())));
        assert_eq!(parse_tag("=orphan"), None);
        assert_eq!(parse_tag("   =x"), None);

        // No separator at all is not a tag.
        assert_eq!(parse_tag("justwords"), None);
    }

    #[test]
    fn delete_dialog_names_a_single_victim_but_counts_a_crowd() {
        assert_eq!(delete_title(&[entry("a/report.txt", false, 0)]), "Xoá report.txt?");
        assert_eq!(delete_title(&[entry("a/logs", true, 0)]), "Xoá logs?");
        assert_eq!(
            delete_title(&[entry("a.txt", false, 0), entry("b.txt", false, 0)]),
            "Xoá 2 mục?"
        );
    }

    #[test]
    fn delete_dialog_warns_about_folders_and_tells_the_truth_about_versioning() {
        // Plain objects in a plain bucket: gone means gone.
        let files = [entry("a.txt", false, 0)];
        assert_eq!(delete_detail(&files, false), "Không hoàn tác được.");

        // A folder stands for everything under it, which the listing does not show.
        let with_folder = [entry("a.txt", false, 0), entry("logs", true, 0)];
        let detail = delete_detail(&with_folder, false);
        assert!(detail.starts_with("Gồm 1 thư mục"), "{detail}");
        assert!(detail.contains("bên trong cũng bị xoá"), "{detail}");

        // Versioned buckets must not be told "cannot be undone" — it is false,
        // and it hides that the old versions keep costing money.
        let versioned = delete_detail(&files, true);
        assert!(versioned.contains("delete marker"), "{versioned}");
        assert!(
            !versioned.contains("Không hoàn tác được"),
            "a versioned delete is reversible: {versioned}"
        );
    }

    #[test]
    fn entry_name_is_the_last_segment_with_or_without_a_trailing_slash() {
        assert_eq!(entry_name_of("reports/q1.txt"), "q1.txt");
        assert_eq!(entry_name_of("reports/2026/"), "2026");
        assert_eq!(entry_name_of("top.txt"), "top.txt");
        assert_eq!(entry_name_of("solo/"), "solo");
    }

    #[test]
    fn renaming_replaces_the_last_segment_and_keeps_the_parent() {
        // A file at the root and a file in a folder.
        assert_eq!(
            renamed_key("reports/q1.txt", "q2.txt").as_deref(),
            Some("reports/q2.txt")
        );
        assert_eq!(renamed_key("q1.txt", "q2.txt").as_deref(), Some("q2.txt"));

        // A folder must keep its trailing slash — without it the prefix turns
        // into a zero-byte object and the folder's contents are orphaned.
        assert_eq!(
            renamed_key("a/b/old/", "new").as_deref(),
            Some("a/b/new/")
        );
        assert_eq!(renamed_key("old/", "new").as_deref(), Some("new/"));

        // Names that would move the entry elsewhere, or nowhere, are refused.
        assert_eq!(renamed_key("a/b.txt", ""), None);
        assert_eq!(renamed_key("a/b.txt", "   "), None);
        assert_eq!(renamed_key("a/b.txt", "c/d.txt"), None);
        assert_eq!(renamed_key("a/old/", "x/y"), None);

        // Surrounding whitespace is trimmed rather than becoming part of the key.
        assert_eq!(
            renamed_key("a/b.txt", "  c.txt  ").as_deref(),
            Some("a/c.txt")
        );
    }

    #[test]
    fn abort_summary_reports_partial_failures_honestly() {
        assert_eq!(abort_summary(3, 0), "Đã huỷ 3 upload dở");
        // The success count is what landed, not the total.
        assert_eq!(abort_summary(3, 1), "Đã huỷ 2/3 upload dở, 1 lỗi");
        // Claiming a partial win when nothing worked would be a lie.
        assert_eq!(abort_summary(3, 3), "Không huỷ được upload nào (3 lỗi)");
        // A task that died before reporting must not underflow.
        assert_eq!(abort_summary(0, 0), "Đã huỷ 0 upload dở");
        assert_eq!(abort_summary(1, 5), "Không huỷ được upload nào (5 lỗi)");
    }
}
