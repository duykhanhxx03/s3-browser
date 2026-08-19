//! The browser window: profiles and buckets on the left, one prefix listed on
//! the right, with sorting, filtering, paging and folder operations.

use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, uniform_list, App, ClickEvent, Context, Entity, ExternalPaths,
    FocusHandle, KeyDownEvent, Modifiers, SharedString, Subscription, Task,
    UniformListScrollHandle, Window,
};
use gpui_component::input::{Input, InputState};
use gpui_tokio::Tokio;
use s3core::{
    format_size, format_timestamp, restore_state, sort_entries, Entry, ObjectHead,
    Encryption, ObjectVersion, OrphanedUpload, Profile, RestoreState, S3Client, Sort, SortKey,
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

/// The one input that still belongs inline: a filter narrows what is already on
/// screen, so putting it in a dialog would hide the thing being filtered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    Filter,
}

/// What a form asks for.
///
/// Everything here used to type into a single unlabelled bar at the top of the
/// window. That bar had no title, no cancel button and nowhere to report a bad
/// value — so what it wanted had to be guessed from a placeholder, and a
/// mistake was only discovered after pressing Enter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormKind {
    NewProfile,
    NewFolder,
    NewBucket,
    /// Carries the key, so a rename still targets the right object if the
    /// selection changes while the dialog is open.
    Rename(String),
    OpenBucket,
    AddTag,
    KmsKey,
    AssumeRole,
    SsoStart,
}

impl FormKind {
    fn title(&self) -> &'static str {
        match self {
            FormKind::NewProfile => "Profile mới",
            FormKind::NewFolder => "Thư mục mới",
            FormKind::NewBucket => "Bucket mới",
            FormKind::Rename(_) => "Đổi tên",
            FormKind::OpenBucket => "Mở bucket",
            FormKind::AddTag => "Thẻ mới",
            FormKind::KmsKey => "Mã hoá KMS",
            FormKind::AssumeRole => "Nhận role",
            FormKind::SsoStart => "Đăng nhập SSO",
        }
    }

    /// Label and placeholder for each field. One entry means a single-field
    /// dialog, which is most of them.
    fn fields(&self) -> Vec<(&'static str, &'static str, bool)> {
        match self {
            FormKind::NewProfile => vec![
                ("Tên", "R2 của tôi", false),
                ("Endpoint", "để trống nếu là AWS", false),
                ("Region", "us-east-1", false),
                ("Access key", "", false),
                ("Secret key", "", true),
            ],
            FormKind::NewFolder => vec![("Tên", "", false)],
            FormKind::NewBucket => vec![("Tên", "", false)],
            FormKind::Rename(_) => vec![("Tên mới", "", false)],
            FormKind::OpenBucket => vec![("Bucket", "", false)],
            FormKind::AddTag => vec![("Khoá", "", false), ("Giá trị", "", false)],
            FormKind::KmsKey => vec![("Key id", "", false)],
            FormKind::AssumeRole => vec![
                ("Role ARN", "arn:aws:iam::…:role/…", false),
                ("MFA serial", "không bắt buộc", false),
                ("Mã MFA", "không bắt buộc", false),
            ],
            FormKind::SsoStart => vec![
                ("Portal URL", "https://…/start", false),
                ("Region", "us-east-1", false),
            ],
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
    /// Set when the thing being confirmed is one version rather than a set of
    /// entries: (key, version id).
    version: Option<(String, String)>,
    /// Set when the whole bucket is to be emptied.
    empty_bucket: Option<SharedString>,
    /// Set when a profile is being removed, by index.
    profile: Option<usize>,
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
    /// Only populated for versioned buckets — asking elsewhere is a request that
    /// can only ever come back with the one version you already know about.
    versions: Vec<ObjectVersion>,
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

/// An in-flight SSO sign-in. The device flow is two waits with a browser trip
/// in between, so the panel has to survive both.
pub struct SsoFlow {
    region: String,
    /// Shown while waiting, so the person can finish in the browser.
    user_code: SharedString,
    verification_uri: SharedString,
    waiting: bool,
    access_token: Option<String>,
    roles: Vec<s3core::sso::SsoRole>,
}

/// One field in a form. The value lives in the component library's
/// [`InputState`] rather than here — that entity is what owns the cursor, the
/// selection and the edit history.
pub struct Field {
    label: &'static str,
    state: Entity<InputState>,
}

/// The add-profile form.
///
/// Every field is a real text input: cursor keys, selection, ⌘A, ⌘V, undo and
/// IME all work because the component library implements them, not because this
/// file re-derives them. The hand-rolled version this replaces had none of that
/// — and its absence had quietly shaped the app, because with only one usable
/// field there was no way to build a form at all.
pub struct Form {
    kind: FormKind,
    fields: Vec<Field>,
    error: Option<SharedString>,
}

impl Form {
    fn new(kind: FormKind, window: &mut Window, cx: &mut App) -> Self {
        let mut fields = Vec::new();
        // A loop rather than a closure: each InputState needs the window
        // mutably, so a closure capturing it could not be called twice.
        for (label, placeholder, masked) in kind.fields() {
            let state = cx.new(|cx| {
                let state = InputState::new(window, cx).placeholder(placeholder);
                // Masked hides the value behind dots; someone adding a profile
                // may well be screen-sharing.
                if masked {
                    state.masked(true)
                } else {
                    state
                }
            });
            fields.push(Field { label, state });
        }

        Self {
            kind,
            fields,
            error: None,
        }
    }

    fn value(&self, label: &str, cx: &App) -> String {
        self.fields
            .iter()
            .find(|field| field.label == label)
            .map(|field| field.state.read(cx).value().trim().to_string())
            .unwrap_or_default()
    }

    /// The first field's value, for the single-field dialogs.
    fn first(&self, cx: &App) -> String {
        self.fields
            .first()
            .map(|field| field.state.read(cx).value().trim().to_string())
            .unwrap_or_default()
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

    orphans: Vec<OrphanedUpload>,
    orphans_open: bool,

    confirm: Option<Confirm>,
    /// An SSO sign-in in progress, and the roles it turned up once done.
    sso: Option<SsoFlow>,
    /// The add-profile form, when open.
    form: Option<Form>,
    /// The command palette: `Some` with the query typed so far.
    palette: Option<String>,
    /// Which row the palette has highlighted.
    palette_selected: usize,
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
            form: None,
            sso: None,
            palette: None,
            palette_selected: 0,
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
            // Not `?`: a token scoped to one bucket — the normal, recommended
            // setup on R2 — is denied ListBuckets while the bucket itself works
            // fine. Failing the whole connection here left those users with no
            // way in at all.
            let buckets = client.list_buckets().await;
            anyhow::Ok((client, buckets))
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = connecting.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok((client, listed))) => {
                        let buckets = match listed {
                            Ok(buckets) => {
                                this.status = format!("{} bucket", buckets.len()).into();
                                buckets
                            }
                            Err(error) => {
                                debug_log!("ListBuckets failed: {error}");
                                // Say what to do next, not just what broke.
                                this.status =
                                    "Không liệt kê được bucket — token có thể chỉ có quyền trên một bucket. Bấm + ở BUCKETS để mở theo tên."
                                        .into();
                                Vec::new()
                            }
                        };
                        debug_log!("connected: {} buckets {:?}", buckets.len(), buckets);
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
        // Reached by name or by `--open`, it still belongs in the list — the
        // sidebar was empty while the user was plainly inside a bucket.
        if !self.buckets.contains(&bucket) {
            self.buckets.push(bucket.clone());
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
            version: None,
            empty_bucket: None,
            profile: None,
        });
        cx.notify();
    }

    /// Starts the SSO device flow: register, get a code, open the browser, then
    /// poll until the person approves.
    fn start_sso(&mut self, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        let (start_url, region) = match text.split_once(char::is_whitespace) {
            Some((url, region)) => (url.trim().to_string(), region.trim().to_string()),
            // Identity Center lives in one region per organisation, and it is
            // not necessarily where the buckets are.
            None => (text, "us-east-1".to_string()),
        };
        if !start_url.starts_with("http") {
            self.report("Cần URL portal, ví dụ: https://tên.awsapps.com/start".into());
            return;
        }

        self.status = "Đang bắt đầu đăng nhập SSO…".into();
        let region_for_flow = region.clone();

        let beginning = Tokio::spawn(cx, async move {
            s3core::sso::begin(&start_url, &region).await
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = beginning.await;
            let authorization = match outcome {
                Ok(Ok(authorization)) => authorization,
                Ok(Err(error)) => {
                    _ = this.update(cx, |this, cx| {
                        this.report(format!("{error}"));
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    _ = this.update(cx, |this, cx| {
                        this.report(format!("Task lỗi: {error}"));
                        cx.notify();
                    });
                    return;
                }
            };

            // Open the browser for them, but keep showing the URL and code: the
            // browser may be the wrong one, or may not open at all.
            _ = opener::open(&authorization.verification_uri);
            _ = this.update(cx, |this, cx| {
                this.sso = Some(SsoFlow {
                    region: region_for_flow.clone(),
                    user_code: authorization.user_code.clone().into(),
                    verification_uri: authorization.verification_uri.clone().into(),
                    waiting: true,
                    access_token: None,
                    roles: Vec::new(),
                });
                cx.notify();
            });

            this.update(cx, |this, cx| this.poll_sso(authorization, cx)).ok();
        }));
    }

    fn poll_sso(&mut self, authorization: s3core::sso::DeviceAuthorization, cx: &mut Context<Self>) {
        let Some(flow) = self.sso.as_ref() else {
            return;
        };
        let region = flow.region.clone();

        let polling = Tokio::spawn(cx, async move {
            let token = s3core::sso::wait_for_token(&authorization, &region).await?;
            let roles = s3core::sso::list_roles(&token, &region).await?;
            anyhow::Ok((token, roles))
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = polling.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok((token, roles))) => {
                        if let Some(flow) = this.sso.as_mut() {
                            flow.waiting = false;
                            flow.access_token = Some(token);
                            flow.roles = roles;
                        }
                        this.status = "Đã đăng nhập — chọn role".into();
                    }
                    Ok(Err(error)) => {
                        this.sso = None;
                        this.report(format!("{error}"));
                    }
                    Err(error) => {
                        this.sso = None;
                        this.report(format!("Task lỗi: {error}"));
                    }
                }
                cx.notify();
            });
        }));
    }

    fn use_sso_role(&mut self, role: s3core::sso::SsoRole, cx: &mut Context<Self>) {
        let Some(flow) = self.sso.as_ref() else {
            return;
        };
        let (Some(token), region) = (flow.access_token.clone(), flow.region.clone()) else {
            return;
        };
        // Region for the buckets, which the profile already knows; the SSO
        // region only governs the sign-in endpoints.
        let base = self
            .active_profile
            .and_then(|ix| self.profiles.get(ix))
            .map(|stored| (stored.region.clone(), stored.endpoint.clone()))
            .unwrap_or_else(|| ("us-east-1".to_string(), None));

        self.status = format!("Đang lấy credentials cho {}…", role.role_name).into();

        let fetching = Tokio::spawn(cx, async move {
            let credentials = s3core::sso::credentials_for(&token, &role, &region).await?;
            let profile = Profile {
                name: role.label(),
                endpoint: base.1,
                region: base.0,
                path_style: false,
                access_key: credentials.access_key.clone(),
                secret_key: credentials.secret_key.clone(),
                session_token: Some(credentials.session_token.clone()),
                relaxed_checksums: false,
            };
            let client = S3Client::connect(&profile).await?;
            let buckets = client.list_buckets().await?;
            anyhow::Ok((client, buckets))
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = fetching.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok((client, buckets))) => {
                        this.client = Some(client);
                        this.buckets = buckets.into_iter().map(SharedString::from).collect();
                        this.bucket = None;
                        this.entries.clear();
                        this.visible.clear();
                        this.sso = None;
                        this.status = "Đã đăng nhập bằng SSO".into();
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
    }

    /// Swaps the session for one under an assumed role. The base profile keeps
    /// its long-lived keys — the temporary credentials live only in this
    /// session, so quitting the app is enough to drop them.
    fn assume_role(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(stored) = self
            .active_profile
            .and_then(|ix| self.profiles.get(ix))
            .cloned()
        else {
            return;
        };
        let Some(request) = parse_assume_role(&text) else {
            self.report("Cần role ARN, ví dụ: arn:aws:iam::123:role/tên".into());
            return;
        };

        let secret = match vault::secret_key(&stored.id) {
            Ok(secret) => secret,
            Err(error) => {
                self.report(format!("Không đọc được khoá bí mật: {error}"));
                return;
            }
        };

        let base = Profile {
            name: stored.name.clone(),
            endpoint: stored.endpoint.clone(),
            region: stored.region.clone(),
            path_style: stored.path_style,
            access_key: stored.access_key.clone(),
            secret_key: secret,
            session_token: vault::session_token(&stored.id),
            relaxed_checksums: stored.relaxed_checksums,
        };

        self.status = format!("Đang nhận role {}…", request.role_arn).into();

        let assuming = Tokio::spawn(cx, async move {
            let credentials = s3core::sts::assume_role(&base, &request).await?;
            let profile = s3core::sts::profile_with(&base, &credentials);
            let client = S3Client::connect(&profile).await?;
            let buckets = client.list_buckets().await?;
            anyhow::Ok((client, buckets, credentials.expires_at))
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = assuming.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok((client, buckets, expires_at))) => {
                        this.client = Some(client);
                        this.buckets = buckets.into_iter().map(SharedString::from).collect();
                        this.bucket = None;
                        this.entries.clear();
                        this.visible.clear();
                        // Saying when it runs out matters: everything signed with
                        // this session, presigned URLs included, dies with it.
                        this.status = match expires_at {
                            Some(at) => format!(
                                "Đã nhận role — phiên hết hạn {}",
                                s3core::format_timestamp(
                                    at.duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs() as i64)
                                        .unwrap_or(0)
                                )
                            )
                            .into(),
                            None => "Đã nhận role".into(),
                        };
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
    }

    fn open_form(&mut self, kind: FormKind, window: &mut Window, cx: &mut Context<Self>) {
        self.form = Some(Form::new(kind, window, cx));
        cx.notify();
    }

    fn submit_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let kind = form.kind.clone();
        let first = form.first(cx);

        // Every single-field dialog rejects an empty value the same way, so the
        // button never silently does nothing.
        if first.is_empty() && kind != FormKind::NewProfile {
            if let Some(form) = self.form.as_mut() {
                form.error = Some("Chưa nhập gì".into());
            }
            cx.notify();
            return;
        }

        match kind {
            FormKind::NewProfile => return self.submit_profile_form(cx),
            FormKind::NewFolder => {
                self.form = None;
                self.create_folder(first, cx);
            }
            FormKind::NewBucket => {
                self.form = None;
                self.create_bucket(first, cx);
            }
            FormKind::Rename(key) => {
                self.form = None;
                self.rename_entry(key, first, cx);
            }
            FormKind::OpenBucket => {
                self.form = None;
                let bucket = SharedString::from(first);
                self.open(bucket, String::new(), cx);
            }
            FormKind::AddTag => {
                let value = form.value("Giá trị", cx);
                self.form = None;
                self.add_tag(format!("{first}={value}"), cx);
            }
            FormKind::KmsKey => {
                self.form = None;
                if let Some(client) = self.client.as_ref() {
                    client.set_encryption(Encryption::Kms(first));
                    self.status = "Mã hoá: SSE-KMS".into();
                }
            }
            FormKind::AssumeRole => {
                let serial = form.value("MFA serial", cx);
                let code = form.value("Mã MFA", cx);
                self.form = None;
                let text = if serial.is_empty() || code.is_empty() {
                    first
                } else {
                    format!("{first} mfa:{serial} {code}")
                };
                self.assume_role(text, cx);
            }
            FormKind::SsoStart => {
                let region = form.value("Region", cx);
                self.form = None;
                self.start_sso(format!("{first} {region}").trim().to_string(), cx);
            }
        }
        cx.notify();
    }

    fn submit_profile_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let name = form.value("Tên", cx);
        let endpoint = form.value("Endpoint", cx);
        let region = form.value("Region", cx);
        let access_key = form.value("Access key", cx);
        let secret_key = form.value("Secret key", cx);

        // Check everything before touching the keychain, so a rejected form
        // never leaves a half-made profile behind.
        let taken: Vec<&str> = self.profiles.iter().map(|p| p.name.as_str()).collect();
        if let Some(message) = validate_profile(&name, &access_key, &secret_key, &taken) {
            if let Some(form) = self.form.as_mut() {
                form.error = Some(message.into());
            }
            cx.notify();
            return;
        }

        // A pasted endpoint often carries the bucket; keep it to open later
        // rather than letting it break the connection.
        let (endpoint, bucket_hint) = if endpoint.is_empty() {
            (String::new(), None)
        } else {
            split_endpoint(&endpoint)
        };

        let stored = StoredProfile {
            id: vault::new_profile_id(&name, &self.profiles),
            name,
            endpoint: (!endpoint.is_empty()).then_some(endpoint),
            region: if region.is_empty() {
                "us-east-1".into()
            } else {
                region
            },
            path_style: false,
            relaxed_checksums: false,
            access_key,
        }
        // Reads the endpoint and sets the provider quirks: R2 wants region
        // `auto`, self-hosted stores want path-style, non-AWS wants relaxed
        // checksums. Getting these wrong looks like a credentials problem.
        .with_provider_defaults();

        self.form = None;
        self.add_profile(stored, &secret_key, cx);
        if let Some(bucket) = bucket_hint {
            // The connection is still in flight; remembering it here means the
            // sidebar has something even when ListBuckets is denied.
            let bucket = SharedString::from(bucket);
            if !self.buckets.contains(&bucket) {
                self.buckets.push(bucket);
            }
        }
    }

    /// Removes a profile and the secret behind it. Asking first because the
    /// secret cannot be recovered from the app once the keychain entry is gone.
    fn ask_remove_profile(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.get(index) else {
            return;
        };
        self.confirm = Some(Confirm {
            title: format!("Xoá profile {}?", profile.name).into(),
            detail: "Xoá cả khoá trong Keychain. Dữ liệu trên S3 giữ nguyên.".into(),
            doomed: Vec::new(),
            version: None,
            empty_bucket: None,
            profile: Some(index),
        });
        cx.notify();
    }

    fn remove_profile(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.profiles.len() {
            return;
        }
        let profile = self.profiles.remove(index);
        if let Err(error) = vault::delete_secret_key(&profile.id) {
            self.report(format!("Không xoá được khoá: {error}"));
        }
        self.save_profiles();

        // Disconnect if the profile being used is the one that just went away.
        if self.active_profile == Some(index) {
            self.client = None;
            self.buckets.clear();
            self.bucket = None;
            self.entries.clear();
            self.visible.clear();
            self.active_profile = None;
            self.status = "Đã xoá profile đang dùng".into();
        } else if let Some(active) = self.active_profile {
            // Indices after the removed one shift down by one.
            if active > index {
                self.active_profile = Some(active - 1);
            }
        }
        cx.notify();
    }

    fn open_palette(&mut self, cx: &mut Context<Self>) {
        self.palette = Some(String::new());
        self.palette_selected = 0;
        cx.notify();
    }

    /// Commands whose label matches what has been typed.
    fn palette_matches(&self) -> Vec<Command> {
        let query = self.palette.clone().unwrap_or_default();
        Command::all()
            .into_iter()
            .filter(|command| command_matches(command.label().0, &query))
            .collect()
    }

    fn run_command(
        &mut self,
        command: Command,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        self.palette = None;
        match command {
            Command::Refresh => {
                if let (Some(bucket), prefix) = (self.bucket.clone(), self.prefix.clone()) {
                    self.open(bucket, prefix, cx);
                }
            }
            Command::GoUp => self.go_up(cx),
            Command::Filter => self.start_prompt(Prompt::Filter, cx),
            Command::NewFolder => {
                if let Some(window) = window {
                    self.open_form(FormKind::NewFolder, window, cx);
                }
            }
            Command::NewBucket => {
                if let Some(window) = window {
                    self.open_form(FormKind::NewBucket, window, cx);
                }
            }
            Command::Rename => {
                if let Some(window) = window {
                    self.start_rename(window, cx);
                }
            }
            Command::Share => self.start_share(cx),
            Command::Inspect => self.toggle_inspector(cx),
            Command::Preview => self.quick_look(cx),
            Command::OpenExternally => {
                // Needs the inspector's loaded head to know the size, so open it
                // first if the user came straight from the palette.
                if self.inspector.is_none() {
                    self.open_inspector(cx);
                } else {
                    self.open_externally(cx);
                }
            }
            Command::Download => self.download_selection(cx),
            Command::Delete => self.ask_delete_selection(cx),
            Command::ToggleQueue => {
                self.drawer_open = !self.drawer_open;
            }
            Command::ScanOrphans => self.scan_orphans(cx),
            Command::EmptyBucket => self.ask_empty_bucket(cx),
            Command::AssumeRole => {
                if let Some(window) = window {
                    self.open_form(FormKind::AssumeRole, window, cx);
                }
            }
            Command::SsoSignIn => {
                if let Some(window) = window {
                    self.open_form(FormKind::SsoStart, window, cx);
                }
            }
            Command::NewProfile => {
                if let Some(window) = window {
                    self.open_form(FormKind::NewProfile, window, cx);
                }
            }
        }
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
        match (confirm.version, confirm.empty_bucket, confirm.profile) {
            (Some((key, version_id)), _, _) => self.delete_version(key, version_id, cx),
            (_, Some(bucket), _) => self.empty_bucket(bucket, cx),
            (_, _, Some(index)) => self.remove_profile(index, cx),
            _ => self.delete_entries(confirm.doomed, cx),
        }
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
            versions: Vec::new(),
        });
        self.load_inspection(key, cx);
        cx.notify();
    }

    fn load_inspection(&mut self, key: String, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };

        let versioned = self.bucket_versioned;
        let loading = Tokio::spawn(cx, async move {
            let head = client.head_object(&bucket, &key).await;
            // Tagging is a separate request, and a provider that does not
            // implement it must not blank out the metadata that did load.
            let tags = client.object_tags(&bucket, &key).await.unwrap_or_default();
            let versions = if versioned {
                client.list_versions(&bucket, &key).await.unwrap_or_default()
            } else {
                Vec::new()
            };
            head.map(|head| (head, tags, versions))
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = loading.await;
            _ = this.update(cx, |this, cx| {
                if let Some(inspector) = this.inspector.as_mut() {
                    inspector.loading = false;
                    match outcome {
                        Ok(Ok((head, tags, versions))) => {
                            inspector.head = Some(head);
                            inspector.tags = tags;
                            inspector.versions = versions;
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

    /// The most destructive thing the app can do, so the dialog names the bucket
    /// and says plainly that versions go too.
    fn ask_empty_bucket(&mut self, cx: &mut Context<Self>) {
        let Some(bucket) = self.bucket.clone() else {
            return;
        };
        self.confirm = Some(Confirm {
            title: format!("Dọn sạch {bucket}?").into(),
            detail: if self.bucket_versioned {
                "Xoá mọi object và mọi phiên bản. Vĩnh viễn.".into()
            } else {
                "Xoá mọi object. Không hoàn tác được.".into()
            },
            doomed: Vec::new(),
            version: None,
            empty_bucket: Some(bucket),
            profile: None,
        });
        cx.notify();
    }

    fn empty_bucket(&mut self, bucket: SharedString, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.status = format!("Đang dọn {bucket}…").into();
        let reopen = bucket.clone();

        let emptying = Tokio::spawn(cx, async move {
            client.empty_bucket(&bucket, |_, _| {}).await
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = emptying.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(report)) if report.errors.is_empty() => {
                        this.status = format!("Đã xoá {} mục", report.deleted).into();
                    }
                    Ok(Ok(report)) => this.report(format!(
                        "Xoá {} mục, {} lỗi: {}",
                        report.deleted,
                        report.errors.len(),
                        report.errors.first().cloned().unwrap_or_default()
                    )),
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                this.open(reopen, String::new(), cx);
                cx.notify();
            });
        }));
    }

    fn restore_version(&mut self, version_id: String, cx: &mut Context<Self>) {
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
            client.restore_version(&bucket, &key, &version_id).await
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = restoring.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(())) => {
                        this.status = "Đã khôi phục version — bản cũ vẫn còn trong lịch sử".into();
                        this.load_inspection(reload, cx);
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
    }

    /// Deleting one version removes data permanently, which an ordinary delete
    /// in a versioned bucket does not — so this one asks first.
    fn ask_delete_version(&mut self, version: ObjectVersion, cx: &mut Context<Self>) {
        let what = if version.is_delete_marker {
            "delete marker"
        } else {
            "version"
        };
        self.confirm = Some(Confirm {
            title: format!("Xoá hẳn {what} này?").into(),
            detail: if version.is_delete_marker {
                "Xoá delete marker sẽ làm object hiện lại như trước khi bị xoá.".into()
            } else {
                "Version bị xoá là mất hẳn, khác với xoá thường trong bucket versioning.".into()
            },
            doomed: Vec::new(),
            version: Some((version.key, version.version_id)),
            empty_bucket: None,
            profile: None,
        });
        cx.notify();
    }

    fn delete_version(&mut self, key: String, version_id: String, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        let reload = key.clone();

        let deleting = Tokio::spawn(cx, async move {
            client.delete_version(&bucket, &key, &version_id).await
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = deleting.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(())) => {
                        this.status = "Đã xoá version".into();
                        this.load_inspection(reload, cx);
                    }
                    Ok(Err(error)) => this.report(format!("{error}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
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
    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut selected = self.selection.iter();
        let (Some(key), None) = (selected.next(), selected.next()) else {
            return;
        };
        let key = key.clone();
        self.open_form(FormKind::Rename(key), window, cx);
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
        self.prompt_text = self.filter.clone();
        self.prompt = Some(prompt);
        cx.notify();
    }

    fn commit_prompt(&mut self, cx: &mut Context<Self>) {
        // The filter is applied while typing, so committing just dismisses it.
        self.prompt = None;
        self.prompt_text.clear();
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

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let primary = is_primary(&keystroke.modifiers);

        // No form branch here on purpose: when a field is focused the Input
        // element receives keys itself. Intercepting them here would break
        // exactly the editing this integration exists to provide. Escape and
        // Enter are handled below, where they do not conflict with typing.
        if self.form.is_some() {
            match keystroke.key.as_str() {
                "escape" => {
                    self.form = None;
                    cx.notify();
                    return;
                }
                // Enter reaches here only when no field consumed it.
                "enter" => return self.submit_form(cx),
                _ => return,
            }
        }

        if let Some(query) = self.palette.clone() {
            let matches = self.palette_matches();
            match keystroke.key.as_str() {
                "escape" => {
                    self.palette = None;
                    cx.notify();
                    return;
                }
                "enter" => {
                    if let Some(command) = matches.get(self.palette_selected).copied() {
                        self.run_command(command, Some(window), cx);
                    }
                    return;
                }
                "down" => {
                    // Clamped rather than wrapping: the list is short and
                    // wrapping past the end reads as the selection vanishing.
                    self.palette_selected =
                        (self.palette_selected + 1).min(matches.len().saturating_sub(1));
                    cx.notify();
                    return;
                }
                "up" => {
                    self.palette_selected = self.palette_selected.saturating_sub(1);
                    cx.notify();
                    return;
                }
                "backspace" => {
                    let mut query = query;
                    query.pop();
                    self.palette = Some(query);
                    self.palette_selected = 0;
                    cx.notify();
                    return;
                }
                _ => {
                    if let Some(text) = keystroke.key_char.as_deref() {
                        if !text.is_empty() && !text.chars().any(char::is_control) {
                            self.palette = Some(format!("{query}{text}"));
                            self.palette_selected = 0;
                            cx.notify();
                        }
                    }
                    return;
                }
            }
        }

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
                    return self.open_form(FormKind::NewBucket, window, cx)
                }
                "n" => return self.open_form(FormKind::NewFolder, window, cx),
                "r" => {
                    if let (Some(bucket), prefix) = (self.bucket.clone(), self.prefix.clone()) {
                        self.open(bucket, prefix, cx);
                    }
                    return;
                }
                "d" => return self.download_selection(cx),
                "enter" => return self.start_rename(window, cx),
                "i" => return self.toggle_inspector(cx),
                "k" => return self.open_palette(cx),
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
            .child(
                section_header("PROFILES", "add-profile", theme).on_click(cx.listener(
                    |this, _event, window, cx| this.open_form(FormKind::NewProfile, window, cx),
                )),
            )
            .children(
                self.profiles
                    .iter()
                    .enumerate()
                    .map(|(index, profile)| {
                        let selected = active_profile == Some(index);
                        // The remove button lives on the row rather than behind a
                        // menu: gpui 0.2.2 has no context menu, and a profile you
                        // cannot delete is worse than one that is easy to delete
                        // by accident — the confirmation covers that.
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                sidebar_row(
                                    SharedString::from(format!("profile-{}", profile.id)),
                                    SharedString::from(profile.name.clone()),
                                    selected,
                                    theme,
                                )
                                .flex_1()
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    this.connect(index, cx);
                                })),
                            )
                            .child(
                                icon_button_dyn(
                                    SharedString::from(format!("rm-profile-{}", profile.id)),
                                    "trash",
                                    theme,
                                )
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    this.ask_remove_profile(index, cx)
                                })),
                            )
                    })
                    .collect::<Vec<_>>(),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        action_button("import-aws", "Nhập từ ~/.aws", theme).on_click(
                            cx.listener(|this, _event, _window, cx| this.import_from_aws(cx)),
                        ),
                    )
                    .when(self.profiles.is_empty(), |this| {
                        this.child(
                            action_button("add-minio", "Thêm MinIO local", theme).on_click(
                                cx.listener(|this, _event, _window, cx| {
                                    this.add_minio_dev_profile(cx)
                                }),
                            ),
                        )
                    }),
            )
            .child(div().h(px(8.)))
            .child(
                section_header("BUCKETS", "add-bucket", theme).on_click(cx.listener(
                    |this, _event, window, cx| {
                        if this.client.is_some() {
                            // Opening by name rather than creating: a scoped
                            // token is denied CreateBucket too, and reaching an
                            // existing bucket is the far more common need.
                            this.open_form(FormKind::OpenBucket, window, cx);
                        }
                    },
                )),
            )
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
                icon_button("up", "arrow-up", theme)
                    .on_click(cx.listener(|this, _event, _window, cx| this.go_up(cx))),
            )
            .child(
                icon_button("refresh", "refresh", theme).on_click(cx.listener(
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
            .when(bucket.is_some(), |this| {
                this.child(
                    action_button("new-folder", "Thư mục mới", theme).on_click(cx.listener(
                        |this, _event, window, cx| this.open_form(FormKind::NewFolder, window, cx),
                    )),
                )
            })
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
                        |this, _event, window, cx| this.start_rename(window, cx),
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

    /// What fills the object pane when there is nothing to list. A blank area
    /// with the explanation hidden in the status bar left the user staring at
    /// black — the recovery has to be where they are already looking.
    fn render_empty_state(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.bucket.is_some() || self.client.is_none() {
            return None;
        }
        let theme = self.theme;
        let cannot_list = self.buckets.is_empty();

        Some(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    div()
                        .text_color(theme.text)
                        .child(if cannot_list {
                            "Không liệt kê được bucket"
                        } else {
                            "Chọn một bucket"
                        }),
                )
                .child(
                    div()
                        .max_w(px(440.))
                        .text_xs()
                        .child(wrapped_text(
                            if cannot_list {
                                // Naming the likely cause separates this from
                                // wrong credentials, which look identical.
                                "Token có thể chỉ có quyền trên một bucket."
                            } else {
                                "Chọn ở cột bên trái."
                            },
                            48,
                            theme.text_muted,
                        )),
                )
                .child(
                    action_button("empty-open-bucket", "Mở bucket theo tên", theme).on_click(
                        cx.listener(|this, _event, window, cx| {
                            this.open_form(FormKind::OpenBucket, window, cx)
                        }),
                    ),
                ),
        )
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
                        .when_some(self.client.clone(), |this, client| {
                            let current = client.encryption();
                            this.child(
                                action_button("encryption", "", theme)
                                    .child(SharedString::from(format!(
                                        "Mã hoá: {}",
                                        encryption_label(&current)
                                    )))
                                    .on_click(cx.listener(move |this, _event, window, cx| {
                                        match next_encryption(&current) {
                                            // KMS needs a key id, so it asks
                                            // rather than silently picking one.
                                            None => this.open_form(FormKind::KmsKey, window, cx),
                                            Some(next) => {
                                                if let Some(client) = this.client.as_ref() {
                                                    client.set_encryption(next);
                                                }
                                                cx.notify();
                                            }
                                        }
                                    })),
                            )
                        })
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
                                "Mã hoá",
                                match (&head.encryption, &head.kms_key_id) {
                                    (Some(kind), Some(key)) => format!("{kind} · {key}"),
                                    (Some(kind), None) => kind.clone(),
                                    // Not the same as "unknown": S3 omits the
                                    // header exactly when nothing encrypts it.
                                    (None, _) => "không".into(),
                                },
                                theme,
                            ))
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
                                        cx.listener(|this, _event, window, cx| {
                                            this.open_form(FormKind::AddTag, window, cx)
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
                                            "close",
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
                    .when(!inspector.versions.is_empty(), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(div().text_color(theme.text_faint).child(
                                    SharedString::from(format!(
                                        "PHIÊN BẢN · {}",
                                        inspector.versions.len()
                                    )),
                                ))
                                .children(inspector.versions.iter().map(|version| {
                                    let for_restore = version.version_id.clone();
                                    let for_delete = version.clone();
                                    let when = version
                                        .modified_epoch
                                        .map(format_timestamp)
                                        .unwrap_or_default();

                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex_1()
                                                .overflow_hidden()
                                                // A delete marker holds no data;
                                                // saying so stops the user asking
                                                // to download nothing.
                                                .text_color(if version.is_delete_marker {
                                                    theme.text_faint
                                                } else {
                                                    theme.text
                                                })
                                                .child(SharedString::from(if version
                                                    .is_delete_marker
                                                {
                                                    format!("{when} · delete marker")
                                                } else {
                                                    format!(
                                                        "{when} · {}{}",
                                                        format_size(version.size),
                                                        if version.is_latest {
                                                            " · hiện tại"
                                                        } else {
                                                            ""
                                                        }
                                                    )
                                                })),
                                        )
                                        .when(
                                            !version.is_latest && !version.is_delete_marker,
                                            |row| {
                                                row.child(
                                                    action_button_dyn(
                                                        SharedString::from(format!(
                                                            "restore-{for_restore}"
                                                        )),
                                                        "Khôi phục".into(),
                                                        theme,
                                                    )
                                                    .on_click(cx.listener(
                                                        move |this, _event, _window, cx| {
                                                            this.restore_version(
                                                                for_restore.clone(),
                                                                cx,
                                                            )
                                                        },
                                                    )),
                                                )
                                            },
                                        )
                                        .child(
                                            icon_button_dyn(
                                                SharedString::from(format!(
                                                    "rm-ver-{}",
                                                    for_delete.version_id
                                                )),
                                                "trash",
                                                theme,
                                            )
                                            .on_click(cx.listener(
                                                move |this, _event, _window, cx| {
                                                    this.ask_delete_version(for_delete.clone(), cx)
                                                },
                                            )),
                                        )
                                })),
                        )
                    })
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
                        .child(icon_button("close-inspector", "close", theme).on_click(cx.listener(
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
                        .bg(theme.modal)
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
                                    .child("Credential tạm: link chết khi session hết hạn."),
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

    /// The first-run screen. Without it a fresh install shows an empty list and
    /// a status line, with nothing saying what to do next — the three ways in
    /// exist but are invisible until a profile already exists.
    fn render_onboarding(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.profiles.is_empty() {
            return None;
        }
        let theme = self.theme;

        let step = |title: &'static str, detail: &'static str, button: &'static str, id: &'static str| {
            (title, detail, button, id)
        };

        Some(
            div()
                .id("onboarding")
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(460.))
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(div().text_color(theme.text).child("s3browser"))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.text_muted)
                                        .child("Khoá bí mật lưu trong Keychain."),
                                ),
                        )
                        .children(
                            [
                                step("Nhập từ ~/.aws", "Profile có khoá tĩnh", "Nhập", "onboard-import"),
                                step("Nhập thủ công", "R2, B2, Wasabi, Spaces, MinIO", "Tạo", "onboard-manual"),
                                step("MinIO trên máy", "127.0.0.1:9000", "Tạo", "onboard-minio"),
                                step("Đăng nhập AWS SSO", "Qua trình duyệt", "Đăng nhập", "onboard-sso"),
                            ]
                            .map(|(title, detail, button, id)| {
                                div()
                                    .p_3()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .rounded_lg()
                                    .bg(theme.panel)
                                    .border_1()
                                    .border_color(theme.border)
                                    .child(
                                        div()
                                            .flex_1()
                                            // Without a zero minimum a flex child
                                            // refuses to shrink below its content,
                                            // and the button next to it is pushed
                                            // outside the card.
                                            .min_w(px(0.))
                                            .overflow_hidden()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(div().text_color(theme.text).child(title))
                                            .child(div().text_xs().child(wrapped_text(
                                                detail,
                                                52,
                                                theme.text_muted,
                                            ))),
                                    )
                                    .child(action_button(id, button, theme).on_click(cx.listener(
                                        move |this, _event, window, cx| match id {
                                            "onboard-import" => this.import_from_aws(cx),
                                            "onboard-manual" => {
                                                this.open_form(FormKind::NewProfile, window, cx)
                                            }
                                            "onboard-minio" => this.add_minio_dev_profile(cx),
                                            _ => this.open_form(FormKind::SsoStart, window, cx),
                                        },
                                    )))
                            }),
                        )
                        .when_some(self.error.clone(), |this, error| {
                            this.child(div().text_xs().text_color(theme.danger).child(error))
                        }),
                ),
        )
    }

    fn render_form(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let form = self.form.as_ref()?;
        let theme = self.theme;

        Some(
            div()
                .id("form-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.45))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.form = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("form")
                        .w(px(460.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_lg()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(div().text_color(theme.text).child(form.kind.title()))
                        .children(form.fields.iter().map(|field| {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(88.))
                                        .text_xs()
                                        .text_color(theme.text_faint)
                                        .child(field.label),
                                )
                                // The real thing: cursor, selection, ⌘A, ⌘V,
                                // undo and IME are the widget's, not ours.
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .child(Input::new(&field.state)),
                                )
                        }))
                        .when_some(form.error.clone(), |this, error| {
                            this.child(div().text_xs().text_color(theme.danger).child(error))
                        })
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .justify_end()
                                .child(action_button("form-cancel", "Huỷ", theme).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.form = None;
                                        cx.notify();
                                    }),
                                ))
                                .child(action_button("form-save", "Lưu", theme).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.submit_form(cx)
                                    }),
                                )),
                        ),
                ),
        )
    }

    fn render_sso(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let flow = self.sso.as_ref()?;
        let theme = self.theme;

        Some(
            div()
                .id("sso-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.45))
                .child(
                    div()
                        .id("sso-dialog")
                        .w(px(460.))
                        .max_h(px(420.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_lg()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        .child(div().text_color(theme.text).child("Đăng nhập AWS SSO"))
                        .when(flow.waiting, |this| {
                            this
                                // The browser may not have opened, or opened the
                                // wrong one, so the code and URL stay visible.
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.text_muted)
                                        .child("Duyệt trong trình duyệt:"),
                                )
                                .child(
                                    div()
                                        .p_2()
                                        .rounded_md()
                                        .bg(theme.hover)
                                        .text_color(theme.text)
                                        .child(flow.user_code.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.text_faint)
                                        .child(flow.verification_uri.clone()),
                                )
                        })
                        .when(!flow.waiting, |this| {
                            this.child(
                                div()
                                    .id("sso-roles")
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .max_h(px(260.))
                                    .overflow_hidden()
                                    .children(flow.roles.iter().map(|role| {
                                        let chosen = role.clone();
                                        div()
                                            .id(SharedString::from(format!(
                                                "sso-{}-{}",
                                                role.account_id, role.role_name
                                            )))
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .text_xs()
                                            .text_color(theme.text)
                                            .hover(|style| style.bg(theme.hover))
                                            .child(SharedString::from(role.label()))
                                            .on_click(cx.listener(
                                                move |this, _event, _window, cx| {
                                                    this.use_sso_role(chosen.clone(), cx)
                                                },
                                            ))
                                    }))
                                    .when(flow.roles.is_empty(), |this| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.text_faint)
                                                .child("Tài khoản này không có role nào"),
                                        )
                                    }),
                            )
                        })
                        .child(
                            div().flex().justify_end().child(
                                action_button("sso-cancel", "Huỷ", theme).on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.sso = None;
                                        cx.notify();
                                    },
                                )),
                            ),
                        ),
                ),
        )
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let query = self.palette.as_ref()?;
        let theme = self.theme;
        let matches = self.palette_matches();
        let selected = self.palette_selected;

        Some(
            div()
                .id("palette-scrim")
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.35))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.palette = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("palette")
                        .mt(px(90.))
                        .w(px(440.))
                        .h(px(320.))
                        .flex()
                        .flex_col()
                        .rounded_lg()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(
                            div()
                                .h(px(34.))
                                .px_3()
                                .flex()
                                .items_center()
                                .border_b_1()
                                .border_color(theme.border)
                                .text_color(theme.text)
                                .child(SharedString::from(if query.is_empty() {
                                    "Gõ để tìm lệnh…".to_string()
                                } else {
                                    query.clone()
                                })),
                        )
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .flex()
                                .flex_col()
                                .children(matches.iter().enumerate().map(|(ix, command)| {
                                    let (label, shortcut) = command.label();
                                    let command = *command;
                                    div()
                                        .id(("cmd", ix))
                                        .h(px(26.))
                                        .px_3()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .text_xs()
                                        .when(ix == selected, |row| row.bg(theme.selected))
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_color(theme.text)
                                                .child(label),
                                        )
                                        .child(
                                            div().text_color(theme.text_faint).child(shortcut),
                                        )
                                        .on_click(cx.listener(
                                            move |this, _event, window, cx| {
                                                this.run_command(command, Some(window), cx)
                                            },
                                        ))
                                }))
                                .when(matches.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .p_3()
                                            .text_xs()
                                            .text_color(theme.text_faint)
                                            .child("Không có lệnh nào khớp"),
                                    )
                                }),
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
                        .bg(theme.modal)
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
        let direction = match job.direction {
            transfer::Direction::Upload => "upload",
            transfer::Direction::Download => "download",
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
            .child(div().w(px(18.)).child(icon(direction, theme.text_faint)))
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
                icon_button("remove", "close", theme).on_click(cx.listener(
                    move |this, _event, _window, cx| {
                        this.transfers.remove_job(id);
                        cx.notify();
                    },
                )),
            )
    }

    fn render_prompt(&self) -> Option<impl IntoElement> {
        let theme = self.theme;
        self.prompt.as_ref()?;

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
                        .child("Lọc"),
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
            // Nothing to browse without a profile, and the chrome behind a
            // translucent overlay read as clutter rather than as background.
            .when_some(self.render_onboarding(cx), |this, onboarding| {
                this.child(onboarding)
            })
            .when(!self.profiles.is_empty(), |this| {
                this.child(self.render_toolbar(cx))
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
                            .when_some(self.render_empty_state(cx), |this, empty| {
                                this.child(empty)
                            })
                            .when(self.bucket.is_some(), |this| {
                                this.child(self.render_columns(cx))
                                    .child(self.render_rows(cx))
                            }),
                    )
                    .children(self.render_inspector(cx)),
            )
                    .children(self.render_drawer(cx))
                    .children(self.render_orphans(cx))
            })
            .child(self.render_status(cx))
            .children(self.render_confirm(cx))
            .children(self.render_share(cx))
            .children(self.render_palette(cx))
            .children(self.render_sso(cx))
            .children(self.render_form(cx))
    }
}

// ------------------------------------------------------------------ elements

/// A section label with an add button. The button was the missing half: every
/// way to create a profile or bucket existed only as a keyboard shortcut or as
/// buttons that appeared when the list was empty and vanished once it was not.
fn section_header(
    text: &'static str,
    id: &'static str,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .flex()
        .items_center()
        .rounded_md()
        .hover(|style| style.bg(theme.hover))
        .child(
            div()
                .flex_1()
                .text_xs()
                .text_color(theme.text_faint)
                .child(text),
        )
        .child(icon("plus", theme.text_muted))
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

/// One icon, tinted to `color` and sized to the row it sits in.
///
/// 16px is the size the set is drawn for; scaling a 24-grid stroke much beyond
/// that makes the line weight visibly different from its neighbours.
fn icon(name: &'static str, color: gpui::Hsla) -> impl IntoElement {
    gpui::svg()
        .path(SharedString::from(format!("icons/{name}.svg")))
        .size(px(16.))
        .text_color(color)
}

/// `icon_button` for ids built from data.
fn icon_button_dyn(
    id: SharedString,
    name: &'static str,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .hover(|style| style.bg(theme.hover))
        .child(icon(name, theme.text_muted))
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

fn icon_button(id: &'static str, name: &'static str, theme: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w(px(26.))
        .h(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(|this| this.bg(theme.hover))
        .child(icon(name, theme.text_muted))
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
        // Without this the row is only as wide as its content: `flex_1` on the
        // name has nothing to expand into, so the size and date columns bunch up
        // against the name instead of lining up under their headers.
        .w_full()
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .cursor_pointer()
        .when(selected, |this| this.bg(theme.selected))
        .hover(|this| this.bg(theme.hover))
        .child(
            div()
                .w(px(22.))
                .flex()
                .items_center()
                // Folders lead, so they carry the accent; files stay quiet.
                .child(if entry.is_folder {
                    icon("folder", theme.accent)
                } else {
                    icon("file", theme.text_faint)
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
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

fn encryption_label(encryption: &Encryption) -> String {
    match encryption {
        Encryption::BucketDefault => "theo bucket".into(),
        Encryption::Aes256 => "SSE-S3".into(),
        // The key id can be a full ARN, which would push everything else off the
        // row; the tail is the part that identifies it to a human.
        Encryption::Kms(key) => format!("KMS · {}", key.rsplit('/').next().unwrap_or(key)),
    }
}

/// The next setting when the button is clicked. `None` means the next one needs
/// input first, so the caller has to ask instead of applying it.
fn next_encryption(current: &Encryption) -> Option<Encryption> {
    match current {
        Encryption::BucketDefault => Some(Encryption::Aes256),
        Encryption::Aes256 => None,
        Encryption::Kms(_) => Some(Encryption::BucketDefault),
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

/// Parses the assume-role prompt: a role ARN, optionally followed by
/// `mfa:<serial> <code>`.
///
/// One text field has to carry three values because the app has no multi-field
/// form yet. Both MFA halves are required together — STS rejects a serial
/// without a code with a message that never mentions the code.
fn parse_assume_role(text: &str) -> Option<s3core::sts::AssumeRole> {
    let text = text.trim();
    let (arn, mfa) = match text.split_once(" mfa:") {
        Some((arn, rest)) => (arn.trim(), Some(rest.trim())),
        None => (text, None),
    };
    if arn.is_empty() || !arn.starts_with("arn:") {
        return None;
    }

    let (mfa_serial, mfa_code) = match mfa {
        Some(rest) => match rest.split_once(char::is_whitespace) {
            Some((serial, code)) if !serial.is_empty() && !code.trim().is_empty() => {
                (Some(serial.to_string()), Some(code.trim().to_string()))
            }
            // A serial with no code would fail server-side for reasons the user
            // cannot guess, so refuse it here where the cause is obvious.
            _ => return None,
        },
        None => (None, None),
    };

    Some(s3core::sts::AssumeRole {
        role_arn: arn.to_string(),
        session_name: "s3browser".into(),
        external_id: None,
        mfa_serial,
        mfa_code,
        duration: None,
    })
}

/// Breaks text into lines at word boundaries.
///
/// gpui 0.2.2 wraps by treating any non-ASCII character as a break opportunity,
/// so Vietnamese text splits mid-word — "cấp" comes out as "c" / "ấp", which
/// reads as a rendering fault. Wrapping here and rendering one line per element
/// sidesteps the built-in wrapping entirely.
///
/// `max_chars` is a character budget, not a pixel measurement: the UI font is
/// proportional, so this is an approximation chosen to fit the container rather
/// than an exact fit.
fn wrap_words(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();

    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        let line_len = line.chars().count();

        if !line.is_empty() && line_len + 1 + word_len > max_chars {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        // A word longer than the budget goes on its own line rather than being
        // cut: breaking it is what this function exists to avoid.
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// A block of text wrapped at word boundaries, one element per line.
fn wrapped_text(text: &str, max_chars: usize, color: gpui::Hsla) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .text_color(color)
        .children(
            wrap_words(text, max_chars)
                .into_iter()
                .map(SharedString::from)
                .map(|line| div().child(line)),
        )
}

/// Splits a bucket off the end of an endpoint URL.
///
/// Cloudflare's dashboard shows the S3 endpoint with the bucket already on it
/// (`https://<id>.r2.cloudflarestorage.com/<bucket>`), which is the string
/// people copy. Pasted as-is it breaks: the SDK cannot handle an endpoint with
/// a path prefix (aws-sdk-rust#1387), and the failure looks like bad
/// credentials rather than a bad URL.
///
/// Returns the endpoint without the path, plus the bucket if there was one.
fn split_endpoint(endpoint: &str) -> (String, Option<String>) {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return (trimmed.to_string(), None);
    };

    match rest.split_once('/') {
        Some((host, path)) if !path.is_empty() => {
            // Only a single segment is a bucket; anything deeper is a path
            // prefix this app cannot serve, so leave it for the caller to
            // reject rather than silently guessing.
            let bucket = path.split('/').next().unwrap_or(path);
            (format!("{scheme}://{host}"), Some(bucket.to_string()))
        }
        _ => (format!("{scheme}://{rest}"), None),
    }
}

/// What is wrong with a profile form, or `None` if nothing is.
///
/// Separate from the form itself because the fields now live inside the
/// component library's input state, which needs a window to exist — the rules
/// should stay checkable without one.
fn validate_profile(
    name: &str,
    access_key: &str,
    secret_key: &str,
    taken: &[&str],
) -> Option<&'static str> {
    if name.is_empty() {
        Some("Cần tên profile")
    } else if access_key.is_empty() {
        Some("Cần access key")
    } else if secret_key.is_empty() {
        Some("Cần secret key")
    } else if taken.contains(&name) {
        // Names are how profiles are told apart in the sidebar, so a duplicate
        // makes the list unreadable even though the ids stay unique.
        Some("Đã có profile trùng tên")
    } else {
        None
    }
}

/// Everything the palette can run. One list so the palette, the shortcut hints
/// and the keyboard handler cannot drift apart — adding a command in one place
/// adds it everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Refresh,
    GoUp,
    Filter,
    NewFolder,
    NewBucket,
    Rename,
    Share,
    Inspect,
    Preview,
    OpenExternally,
    Download,
    Delete,
    ToggleQueue,
    ScanOrphans,
    EmptyBucket,
    AssumeRole,
    SsoSignIn,
    NewProfile,
}

impl Command {
    /// Name, and the shortcut that also runs it. `⌘` is spelled out because the
    /// palette is read, not pressed.
    fn label(self) -> (&'static str, &'static str) {
        match self {
            Command::Refresh => ("Tải lại", "⌘R"),
            Command::GoUp => ("Lên một cấp", "⌘↑"),
            Command::Filter => ("Lọc", "⌘F"),
            Command::NewFolder => ("Thư mục mới", "⌘N"),
            Command::NewBucket => ("Bucket mới", "⌘⇧N"),
            Command::Rename => ("Đổi tên", "⌘⏎"),
            Command::Share => ("Chia sẻ / presigned URL", ""),
            Command::Inspect => ("Chi tiết", "⌘I"),
            Command::Preview => ("Xem trước", "Space"),
            Command::OpenExternally => ("Mở bằng app ngoài", ""),
            Command::Download => ("Tải xuống", "⌘D"),
            Command::Delete => ("Xoá mục đã chọn", "⌘⌫"),
            Command::ToggleQueue => ("Hàng đợi truyền tải", "⌘J"),
            Command::ScanOrphans => ("Dọn upload dở", ""),
            Command::EmptyBucket => ("Dọn sạch bucket", ""),
            Command::AssumeRole => ("Nhận role (STS AssumeRole)", ""),
            Command::SsoSignIn => ("Đăng nhập AWS SSO", ""),
            Command::NewProfile => ("Profile mới", ""),
        }
    }

    fn all() -> [Command; 18] {
        [
            Command::Refresh,
            Command::GoUp,
            Command::Filter,
            Command::NewFolder,
            Command::NewBucket,
            Command::Rename,
            Command::Share,
            Command::Inspect,
            Command::Preview,
            Command::OpenExternally,
            Command::Download,
            Command::Delete,
            Command::ToggleQueue,
            Command::ScanOrphans,
            Command::EmptyBucket,
            Command::AssumeRole,
            Command::SsoSignIn,
            Command::NewProfile,
        ]
    }
}

/// Case- and accent-tolerant match. Vietnamese command names are typed without
/// diacritics as often as with them, so "doi ten" has to find "Đổi tên".
fn command_matches(label: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    fold(label).contains(&fold(query))
}

/// Lowercases and strips Vietnamese diacritics, so a query typed on a plain
/// keyboard still matches.
fn fold(text: &str) -> String {
    text.chars()
        .flat_map(|c| c.to_lowercase())
        .map(|c| match c {
            'à' | 'á' | 'ả' | 'ã' | 'ạ' | 'ă' | 'ằ' | 'ắ' | 'ẳ' | 'ẵ' | 'ặ' | 'â' | 'ầ' | 'ấ'
            | 'ẩ' | 'ẫ' | 'ậ' => 'a',
            'è' | 'é' | 'ẻ' | 'ẽ' | 'ẹ' | 'ê' | 'ề' | 'ế' | 'ể' | 'ễ' | 'ệ' => 'e',
            'ì' | 'í' | 'ỉ' | 'ĩ' | 'ị' => 'i',
            'ò' | 'ó' | 'ỏ' | 'õ' | 'ọ' | 'ô' | 'ồ' | 'ố' | 'ổ' | 'ỗ' | 'ộ' | 'ơ' | 'ờ' | 'ớ'
            | 'ở' | 'ỡ' | 'ợ' => 'o',
            'ù' | 'ú' | 'ủ' | 'ũ' | 'ụ' | 'ư' | 'ừ' | 'ứ' | 'ử' | 'ữ' | 'ự' => 'u',
            'ỳ' | 'ý' | 'ỷ' | 'ỹ' | 'ỵ' => 'y',
            'đ' => 'd',
            other => other,
        })
        .collect()
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
        format!("Gồm {folders} {noun}, kể cả nội dung bên trong. ")
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
            form: None,
            sso: None,
            palette: None,
            palette_selected: 0,
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
    fn wrapping_never_splits_a_word() {
        // The bug this exists for: gpui breaks before any non-ASCII character,
        // turning "cấp" into "c" / "ấp". Every line here must be whole words.
        let text = "Token này có thể chỉ có quyền trên một bucket cụ thể — với R2 đó là cách cấp quyền được khuyến nghị.";
        let lines = wrap_words(text, 40);
        assert!(lines.len() > 1, "this text is long enough to wrap");
        for line in &lines {
            assert!(!line.starts_with(' ') && !line.ends_with(' '), "{line:?}");
        }
        // Rejoining the lines must give back exactly the words, in order —
        // nothing dropped and nothing cut in half.
        assert_eq!(
            lines.join(" ").split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>()
        );

        // Short text stays on one line.
        assert_eq!(wrap_words("ngắn", 40), vec!["ngắn"]);

        // A word longer than the budget gets its own line rather than being cut.
        let long = "https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com";
        assert_eq!(wrap_words(long, 20), vec![long]);

        // Empty input still yields one (empty) line, so callers can render
        // unconditionally.
        assert_eq!(wrap_words("", 40), vec![String::new()]);
        assert_eq!(wrap_words("   ", 40), vec![String::new()]);

        // The budget is respected where the words allow it.
        for line in wrap_words(text, 30) {
            let len = line.chars().count();
            assert!(
                len <= 30 || !line.contains(' '),
                "line over budget without being a single long word: {line:?}"
            );
        }
    }

    #[test]
    fn endpoint_with_a_bucket_on_it_is_split_rather_than_broken() {
        // Exactly what Cloudflare's dashboard hands you, and what people paste.
        assert_eq!(
            split_endpoint("https://abc123.r2.cloudflarestorage.com/s3-browser"),
            (
                "https://abc123.r2.cloudflarestorage.com".into(),
                Some("s3-browser".into())
            )
        );

        // A plain endpoint is left alone.
        assert_eq!(
            split_endpoint("https://abc123.r2.cloudflarestorage.com"),
            ("https://abc123.r2.cloudflarestorage.com".into(), None)
        );
        // A trailing slash is not a bucket.
        assert_eq!(
            split_endpoint("http://127.0.0.1:9000/"),
            ("http://127.0.0.1:9000".into(), None)
        );
        // Port survives the split.
        assert_eq!(
            split_endpoint("http://127.0.0.1:9000/demo"),
            ("http://127.0.0.1:9000".into(), Some("demo".into()))
        );
        // Surrounding whitespace from a paste.
        assert_eq!(
            split_endpoint("  https://h/b  "),
            ("https://h".into(), Some("b".into()))
        );
        // Deeper paths take only the first segment as the bucket.
        assert_eq!(
            split_endpoint("https://h/bucket/prefix"),
            ("https://h".into(), Some("bucket".into()))
        );
        // Something with no scheme is passed through untouched rather than
        // mangled into a wrong host.
        assert_eq!(split_endpoint("localhost:9000"), ("localhost:9000".into(), None));
    }

    #[test]
    fn profile_validation_catches_every_way_the_form_can_be_wrong() {
        // The happy path.
        assert_eq!(validate_profile("R2", "AKIA", "secret", &[]), None);

        // Each required field, reported specifically rather than as one vague
        // "invalid" — the point of the message is to say which box to fill.
        assert_eq!(
            validate_profile("", "AKIA", "secret", &[]),
            Some("Cần tên profile")
        );
        assert_eq!(
            validate_profile("R2", "", "secret", &[]),
            Some("Cần access key")
        );
        assert_eq!(
            validate_profile("R2", "AKIA", "", &[]),
            Some("Cần secret key")
        );

        // Duplicate names make the sidebar unreadable even though ids stay
        // unique underneath.
        assert_eq!(
            validate_profile("R2", "AKIA", "secret", &["R2"]),
            Some("Đã có profile trùng tên")
        );
        // A different name alongside an existing one is fine.
        assert_eq!(validate_profile("B2", "AKIA", "secret", &["R2"]), None);
    }

    #[test]
    fn assume_role_prompt_requires_both_mfa_halves() {
        let arn = "arn:aws:iam::123456789012:role/reader";

        // Plain role, no MFA.
        let parsed = parse_assume_role(arn).unwrap();
        assert_eq!(parsed.role_arn, arn);
        assert_eq!(parsed.mfa_serial, None);
        assert_eq!(parsed.mfa_code, None);

        // With MFA, both halves present.
        let parsed =
            parse_assume_role(&format!("{arn} mfa:arn:aws:iam::123:mfa/mai 123456")).unwrap();
        assert_eq!(parsed.role_arn, arn);
        assert_eq!(parsed.mfa_serial.as_deref(), Some("arn:aws:iam::123:mfa/mai"));
        assert_eq!(parsed.mfa_code.as_deref(), Some("123456"));

        // A serial with no code fails server-side with a message that never
        // mentions the code, so it is refused here where the cause is visible.
        assert!(parse_assume_role(&format!("{arn} mfa:arn:aws:iam::123:mfa/mai")).is_none());
        assert!(parse_assume_role(&format!("{arn} mfa:")).is_none());

        // Something that is not an ARN is not a role.
        assert!(parse_assume_role("reader").is_none());
        assert!(parse_assume_role("").is_none());

        // Surrounding whitespace is formatting, not part of the ARN.
        assert_eq!(
            parse_assume_role(&format!("  {arn}  ")).unwrap().role_arn,
            arn
        );
    }

    #[test]
    fn palette_finds_commands_typed_without_diacritics() {
        // The point of folding: a plain keyboard must still reach "Đổi tên".
        assert!(command_matches("Đổi tên", "doi ten"));
        assert!(command_matches("Đổi tên", "Đổi"));
        assert!(command_matches("Xem trước", "xem truoc"));
        assert!(command_matches("Dọn sạch bucket", "don sach"));
        assert!(command_matches("Hàng đợi truyền tải", "hang doi"));

        // Case is ignored in both directions.
        assert!(command_matches("Tải lại", "TAI"));

        // An empty query matches everything, so the palette opens full.
        assert!(command_matches("bất kỳ", ""));

        // Not a substring is not a match.
        assert!(!command_matches("Đổi tên", "xoa"));
    }

    #[test]
    fn every_command_has_a_label_and_the_list_is_complete() {
        let all = Command::all();
        // A command missing from `all()` would be unreachable from the palette
        // while still looking implemented.
        assert_eq!(all.len(), 18);
        for command in all {
            let (label, _) = command.label();
            assert!(!label.is_empty(), "{command:?} has no label");
        }

        // Labels must be distinct, or two rows look like the same command.
        let mut labels: Vec<&str> = all.iter().map(|c| c.label().0).collect();
        labels.sort_unstable();
        let before = labels.len();
        labels.dedup();
        assert_eq!(before, labels.len(), "duplicate command labels");
    }

    #[test]
    fn encryption_cycle_asks_before_it_needs_a_key() {
        assert_eq!(encryption_label(&Encryption::BucketDefault), "theo bucket");
        assert_eq!(encryption_label(&Encryption::Aes256), "SSE-S3");

        // A full ARN would push the rest of the row off screen; the tail is what
        // identifies the key to a person.
        assert_eq!(
            encryption_label(&Encryption::Kms(
                "arn:aws:kms:ap-southeast-1:1234:key/abcd-1234".into()
            )),
            "KMS · abcd-1234"
        );
        // A bare id has no slash to split on and must survive intact.
        assert_eq!(
            encryption_label(&Encryption::Kms("abcd-1234".into())),
            "KMS · abcd-1234"
        );

        // Default → SSE-S3 applies directly.
        assert_eq!(
            next_encryption(&Encryption::BucketDefault),
            Some(Encryption::Aes256)
        );
        // SSE-S3 → KMS cannot be applied without asking for a key id first.
        assert_eq!(next_encryption(&Encryption::Aes256), None);
        // KMS → back to the default, so the cycle always has a way out.
        assert_eq!(
            next_encryption(&Encryption::Kms("k".into())),
            Some(Encryption::BucketDefault)
        );
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
        assert!(detail.contains("nội dung bên trong"), "{detail}");

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
