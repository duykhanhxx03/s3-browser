//! The browser window: profiles and buckets on the left, one prefix listed on
//! the right, with sorting, filtering, paging and folder operations.

use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui::Focusable as _;
use gpui::{
    div, ease_out_quint, point, prelude::*, px, uniform_list, App, ClickEvent, Context, Entity,
    ExternalPaths, FocusHandle, KeyDownEvent, Modifiers, ObjectFit, Pixels, Point, SharedString,
    Subscription, Task, Transition, TransitionExt, UniformListScrollHandle, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt, PopupMenu};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::tooltip::Tooltip;
use gpui_tokio::Tokio;
use s3core::{
    capability::{Capabilities, CapabilityCache, Support},
    format_size, format_timestamp, restore_state, sort_entries, Entry, ObjectAcl, ObjectHead,
    ObjectVersion, Profile, RestoreState, S3Client, Sort, SortKey,
};
use transfer::{Job, JobState, TransferEngine};
use vault::{Place, PlaceStore, Places, ProfileStore, Provider, StoredProfile};

use crate::failure::{Failure, Fix};
use crate::platform::{self, Chrome};
use crate::settings::{MotionChoice, Settings, SettingsStore, ThemeChoice};
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

static REDUCE_MOTION: AtomicBool = AtomicBool::new(false);

fn motion_choice_reduces(choice: MotionChoice) -> bool {
    match choice {
        MotionChoice::System => platform::reduce_motion(),
        MotionChoice::Full => false,
        MotionChoice::Reduced => true,
    }
}

fn set_reduce_motion(reduce: bool) {
    REDUCE_MOTION.store(reduce, Ordering::Relaxed);
}

fn reduce_motion() -> bool {
    REDUCE_MOTION.load(Ordering::Relaxed)
}

fn smooth_transition(duration_ms: u64) -> Transition {
    let duration_ms = if reduce_motion() { 1 } else { duration_ms };
    Transition::new(Duration::from_millis(duration_ms)).with_easing(ease_out_quint())
}

fn delayed_transition(duration_ms: u64, delay_ms: u64) -> Transition {
    let delay_ms = if reduce_motion() { 0 } else { delay_ms };
    smooth_transition(duration_ms).with_delay(Duration::from_millis(delay_ms))
}

fn motion_offset(x: f32, y: f32) -> Point<Pixels> {
    if reduce_motion() {
        point(px(0.0), px(0.0))
    } else {
        point(px(x), px(y))
    }
}

fn row_stagger_from_end(position: usize, end: usize) -> u64 {
    if reduce_motion() {
        0
    } else {
        stagger_from_end(position, end, 11)
    }
}

fn grid_stagger_from_end(position: usize, visible_end: usize) -> u64 {
    if reduce_motion() {
        0
    } else {
        stagger_from_end(position, visible_end, 17)
    }
}

fn stagger_from_end(position: usize, end: usize, cap: usize) -> u64 {
    let cells_from_end = end.saturating_sub(position + 1).min(cap);
    (cells_from_end as u64) * MOTION_STAGGER_MS
}

fn animation_key_hash(key: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

const ROW_HEIGHT: f32 = 28.;
const GRID_COLUMNS: usize = 5;
const GRID_ROW_HEIGHT: f32 = 212.;
const GRID_MEDIA_HEIGHT: f32 = 112.;
/// One scale for every bar and panel, so nothing is a few pixels off its
/// neighbour for no reason. Before this there were six different bar heights
/// and three widths for what is visually the same dialog.
const TOOLBAR_HEIGHT: f32 = 40.;
/// Label-only bars: column headers.
const HEADER_HEIGHT: f32 = 28.;
/// Bars that hold buttons. Taller than a plain header because a button with
/// padding does not fit in 28px — collapsing both into one height made the
/// drawer controls overflow their own bar.
const CONTROL_BAR_HEIGHT: f32 = 34.;
/// A transfer row carries two lines (name and progress), so it is taller.
const JOB_ROW_HEIGHT: f32 = 38.;
/// The drawer and the orphan list.
const PANEL_HEIGHT: f32 = 200.;
const DIALOG_WIDTH: f32 = 460.;
/// Error details are long SDK strings, not prompts. Give them room so the
/// useful part is not split into two-word shards.
const FAILURE_DIALOG_WIDTH: f32 = 640.;
/// The profile dialog is wider: an endpoint URL and a 64-character secret both
/// need room, and eliding either of them in the field where it is being typed
/// is the one place eliding is unacceptable.
const PROFILE_DIALOG_WIDTH: f32 = 560.;
/// The preview overlay. Wide, because the point of taking it out of the 320px
/// inspector was to be able to see the thing.
const PREVIEW_WIDTH: f32 = 980.;
const PREVIEW_HEIGHT: f32 = 560.;
const PREVIEW_TITLE_HEIGHT: f32 = CONTROL_BAR_HEIGHT;
const PREVIEW_SIDE_WIDTH: f32 = 276.;
const PREVIEW_BODY_HEIGHT: f32 = PREVIEW_HEIGHT - PREVIEW_TITLE_HEIGHT;
/// Right-hand inspector.
const INSPECTOR_WIDTH: f32 = 320.;
/// Tall enough for every command without scrolling; it scrolls anyway once a
/// filter is typed and more rows appear than fit.
const PALETTE_HEIGHT: f32 = 452.;
/// Wider than the other dialogs: a palette row carries a label *and* a
/// shortcut chip, and at 460 the two collided on the longest labels.
const PALETTE_WIDTH: f32 = 540.;
/// Roomier than a list row. The palette is the app's front door, not a data
/// grid; cramped rows read as a debug menu.
const PALETTE_ROW_HEIGHT: f32 = 38.;
/// The file-type tile in the preview and detail headers.
///
/// Down from 30. In the detail panel it sits beside a single line of 12px text
/// and a 26px close button, and at 30 it was the tallest thing in the row by
/// half again — a large empty box next to a small label, which is most of why
/// it read as unfinished.
const KIND_TILE: f32 = 26.;
/// The same tile standing in for a whole preview, when the type is one this app
/// hands to another application rather than drawing itself.
const KIND_TILE_LARGE: f32 = 64.;
const PROGRESS_HEIGHT: f32 = 4.;
/// Wide enough for a file name and a size pair, narrow enough to stay out of
/// the way of the listing it floats over.
const OPENING_WIDTH: f32 = 248.;
/// How much of the object one request pulls while fetching it for another app.
///
/// Small enough that the bar moves on anything worth watching, large enough
/// that a 500 MB file is not five hundred round trips.
const OPEN_CHUNK: u64 = 4 * 1024 * 1024;
/// Queue progress is the one motion that should feel alive. 125ms is frequent
/// enough to avoid chunky bars without repainting the whole app every frame.
const TRANSFER_TICK_MS: u64 = 125;
/// Short affordance fades: loading labels, empty states, and small state swaps.
const MOTION_QUICK_MS: u64 = 160;
/// Larger overlays need a hair more time so they arrive without popping.
const MOTION_MODAL_MS: u64 = 180;
/// Side panels are spatial changes; keep them calm and a little slower.
const MOTION_PANEL_MS: u64 = 220;
/// Full-pane swaps should be quieter than dialogs: just enough to avoid a hard
/// cut when moving between Objects, Buckets, and Recent.
const MOTION_PAGE_MS: u64 = 140;
/// Row-level motion needs to be barely there: enough to prevent hard pops, not
/// enough to make a data grid feel theatrical.
const MOTION_ROW_MS: u64 = 120;
const MOTION_STAGGER_MS: u64 = 8;
/// Every button is this tall. Sizing to content let a long label grow taller
/// than the bar holding it, so buttons in the same row did not match.
const BUTTON_HEIGHT: f32 = 22.;
const SIDEBAR_WIDTH: f32 = 214.;
const TAB_HEIGHT: f32 = 30.;
/// Wide enough for four digits, which is where a listing stops being something
/// anyone counts through anyway.
const ROW_NUMBER_WIDTH: f32 = 34.;
/// The type column. Six characters is the longest thing `type_badge` will
/// return, so nothing in it ever needs eliding.
const TYPE_WIDTH: f32 = 52.;
const CHECK_WIDTH: f32 = 22.;
/// How long a bucket list stays good for. Buckets are created rarely and the
/// list costs a request every time a profile is opened.
const BUCKET_CACHE_TTL: i64 = 30 * 60;
/// Enough room for six small icon buttons without eating the name column.
const ACTIONS_WIDTH: f32 = 146.;
/// Fixed, so tabs do not resize under the pointer as their titles change.
const TAB_WIDTH: f32 = 132.;
/// How many trailing prefix levels the breadcrumb draws before it collapses the
/// rest into `…`. Three plus the bucket is four things on screen, which is
/// about what fits beside the filter box and the toolbar buttons.
const CRUMBS_SHOWN: usize = 3;
/// How many places the path box offers. Enough to cover a morning's jumping
/// about, few enough that the list does not cover the thing it is helping you
/// get back to.
const PATH_SUGGESTIONS: usize = 8;
/// Wide enough for `bucket/prefix/deeper/` without being as wide as the window.
const PATH_SUGGEST_WIDTH: f32 = 460.;
/// Wide enough for `21 thg 8 16:44`, which is the longest this-year form.
const BUCKET_CREATED_WIDTH: f32 = 130.;
/// How many failures to keep. A retry loop against a dead endpoint would grow
/// the log without bound otherwise, and nobody reads the fiftieth copy.
const FAILURE_LIMIT: usize = 50;
const FAILURE_DETAIL_HEIGHT: f32 = 92.;
/// The filter field. Wide enough for a real file name, narrow enough that the
/// breadcrumb keeps most of the bar.
const FILTER_WIDTH: f32 = 196.;
/// The narrowest the name column may get. Below this a row stops saying which
/// object it is about, which is the one thing every other column depends on.
const NAME_MIN_WIDTH: f32 = 140.;
/// The label column in the settings panel. Wide enough that a one-line note
/// stays on one line.
const SETTING_LABEL_WIDTH: f32 = 210.;
const FIELD_HEIGHT: f32 = 26.;
/// How many buckets before the sidebar gets a search box. Under this the list
/// fits on screen and the box is pure chrome.
const BUCKET_FILTER_MIN: usize = 10;
/// Start fetching the next page once the viewport comes this close to the end.
const PREFETCH_MARGIN: usize = 40;
/// Thumbnails arrive in small waves. Fetching every visible image in one task
/// makes the list sit still and then repaint a wall of rows at once.
const THUMBNAIL_BATCH: usize = 6;

gpui::actions!(
    s3browser,
    [
        /// Every file operation the context menu offers.
        ///
        /// Real gpui actions rather than closures: the menu component dispatches
        /// actions through the focus tree, and having them named means the menu
        /// and the keyboard reach the same handler instead of two copies that
        /// drift apart.
        ActionCopy,
        ActionCut,
        ActionPaste,
        ActionRename,
        ActionDuplicate,
        ActionDelete,
        ActionDownload,
        ActionShare,
        ActionInspect,
        ActionSelectAll,
        ActionRefresh,
        ActionNewFolder,
        ActionPreview,
        ActionOpenExternally,
        ActionEditHeaders,
        ActionOpenInTab,
        ActionCopyPath,
        ActionCopyKey,
        ActionAclPrivate,
        ActionAclPublic,
    ]
);

/// What a form asks for.
///
/// Everything here used to type into a single unlabelled bar at the top of the
/// window. That bar had no title, no cancel button and nowhere to report a bad
/// value — so what it wanted had to be guessed from a placeholder, and a
/// mistake was only discovered after pressing Enter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormKind {
    NewProfile,
    EditProfile(usize),
    NewFolder,
    NewBucket,
    /// Carries the key, so a rename still targets the right object if the
    /// selection changes while the dialog is open.
    Rename(String),
    /// Duplicating the selected object; carries its key for the same reason as
    /// `Rename`.
    Duplicate(String),
    OpenBucket,
    /// Rewriting HTTP headers. Carries every key it will be applied to, for the
    /// same reason `Rename` carries one: the selection can change while the
    /// dialog is open.
    EditHeaders(Vec<String>),
    AddTag,
    AssumeRole,
    SsoStart,
}

impl FormKind {
    fn title(&self) -> String {
        match self {
            FormKind::NewProfile => "Profile mới",
            FormKind::EditProfile(_) => "Sửa profile",
            FormKind::NewFolder => "Thư mục mới",
            FormKind::NewBucket => "Bucket mới",
            FormKind::Rename(_) => "Đổi tên",
            FormKind::Duplicate(_) => "Sao chép",
            FormKind::OpenBucket => "Mở bucket",
            // The count, because setting a header on four hundred objects and
            // on one look identical once the dialog is open.
            FormKind::EditHeaders(keys) if keys.len() > 1 => {
                return format!("Sửa header cho {} mục", keys.len())
            }
            FormKind::EditHeaders(_) => "Sửa header",
            FormKind::AddTag => "Thẻ mới",
            FormKind::AssumeRole => "Nhận role",
            FormKind::SsoStart => "Đăng nhập SSO",
        }
        .to_string()
    }

    /// Label and placeholder for each field. One entry means a single-field
    /// dialog, which is most of them.
    fn fields(&self) -> Vec<(&'static str, &'static str, bool)> {
        match self {
            FormKind::NewProfile | FormKind::EditProfile(_) => vec![
                ("Tên", "R2 của tôi", false),
                ("Endpoint", "để trống nếu là AWS", false),
                ("Region", "us-east-1", false),
                ("Bucket thử", "nếu token không list bucket", false),
                ("Access key", "", false),
                (
                    "Secret key",
                    if matches!(self, FormKind::EditProfile(_)) {
                        "giữ nguyên nếu để trống"
                    } else {
                        ""
                    },
                    true,
                ),
            ],
            FormKind::NewFolder => vec![("Tên", "", false)],
            FormKind::NewBucket => vec![("Tên", "", false)],
            FormKind::Rename(_) => vec![("Tên mới", "", false)],
            FormKind::Duplicate(_) => vec![("Tên bản sao", "", false)],
            FormKind::OpenBucket => vec![("Bucket", "bucket hoặc s3://bucket/prefix/", false)],
            FormKind::EditHeaders(_) => vec![
                ("Content-Type", "image/png", false),
                ("Cache-Control", "public, max-age=3600", false),
                ("Content-Disposition", "inline", false),
            ],
            FormKind::AddTag => vec![("Khoá", "", false), ("Giá trị", "", false)],
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
    /// Which preset is showing, so the buttons say which one is active rather
    /// than leaving the user to remember what they clicked.
    chosen: &'static str,
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
    /// Only fetched where the bucket actually has ACLs enabled.
    acl: Option<ObjectAcl>,
    loading: bool,
    /// Only populated for versioned buckets — asking elsewhere is a request that
    /// can only ever come back with the one version you already know about.
    versions: Vec<ObjectVersion>,
}

/// A preview, on its own over the window.
///
/// Its own surface rather than a panel inside the inspector, which is 320
/// pixels wide: an image or a table shown in a column that narrow is proof the
/// file exists rather than a look at it.
///
/// Independent of the inspector in state too, and that fixes two things. Space
/// used to preview whatever the inspector already had open, so moving to
/// another object and pressing it showed the previous one's contents. And
/// opening the inspector and previewing in the same gesture read `head` before
/// the HEAD request had returned, so the size came back zero and the preview
/// gave up. The size is in the listing already; nothing needs to be asked.
/// A file being fetched so that another application can open it.
///
/// The counters are shared with the fetch task rather than sent back through
/// the entity on every chunk: the repaint loop already runs at
/// `TRANSFER_TICK_MS` for the transfer queue, and reading two atomics from it
/// is cheaper than waking the view a hundred times a second.
struct Opening {
    name: SharedString,
    /// Where the bytes are landing, so cancelling can clean up after itself.
    path: PathBuf,
    /// Bytes written so far.
    done: std::sync::Arc<AtomicU64>,
    /// The object's size. Zero for the moment before the size is known, which
    /// is why the bar is drawn empty rather than full at that point.
    total: std::sync::Arc<AtomicU64>,
}

pub struct Previewing {
    key: String,
    name: SharedString,
    /// `None` while the bytes are on their way.
    content: Option<Preview>,
    /// The object's size as the listing reported it. Kept because editing is
    /// only safe when the preview holds the *whole* object: saving back a
    /// truncated one would delete everything past the limit.
    size: i64,
    /// The editor, once someone has asked for it.
    editing: Option<Entity<InputState>>,
    /// True while the edit is being written back.
    saving: bool,
}

/// What a preview can show of an object's contents. Only ever holds the first
/// the settings' preview limit: a preview must never become an accidental download
/// of a multi-gigabyte object.
pub enum Preview {
    Image(std::sync::Arc<gpui::Image>),
    Text(SharedString),
    /// Delimited text, laid out in columns.
    Table(Table),
    /// Nothing to draw here, and why.
    ///
    /// One variant rather than several because the panel is the same either
    /// way: say what this is, say why it is not on screen, and offer the app on
    /// the machine that *can* open it. The reason is carried because "không xem
    /// trước được" for a ZIP, a corrupt PNG and a file of raw bytes are three
    /// different facts and the user can act on only one of them.
    Handoff(SharedString),
}

/// Which page the main pane is showing.
///
/// Not per-tab. A tab is a *location* — a bucket and a prefix — and the history
/// is one list across all of them; putting a screen inside a tab would mean
/// each tab remembering whether it was "on the history page", which is a state
/// nobody is keeping track of while they work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Objects,
    Recent,
    Buckets,
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
    /// Whether it holds a secret, so the dialog can offer to reveal it.
    masked: bool,
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
    /// The provider dropdown, on the profile form only.
    provider_select: Option<Entity<SelectState<Vec<&'static str>>>>,
    /// Kept alive so the dropdown keeps reporting what was picked.
    _provider_events: Option<Subscription>,
    /// What a connection test said. Separate from `error`, which is about the
    /// form being wrong: a test that fails leaves the form perfectly valid and
    /// still worth saving, because the endpoint may just be down.
    probe: Option<Probe>,
}

/// The outcome of a "test connection".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    Running,
    /// Reached the endpoint and the signature was accepted.
    Ok(SharedString),
    /// The key signed a real response, but a capability is missing.
    Warning(SharedString),
    /// Did not. The message is already classified.
    Failed(SharedString),
}

enum ProfileProbe {
    Buckets(usize),
    Bucket {
        bucket: String,
        prefix: String,
        entries: usize,
    },
    ListBucketsDenied(String),
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
            fields.push(Field {
                label,
                state,
                masked,
            });
        }

        Self {
            kind,
            fields,
            error: None,
            provider_select: None,
            _provider_events: None,
            probe: None,
        }
    }

    /// Writes a value into one of the fields, for the preset buttons.
    /// Same as [`Self::set`] for a value that is not a literal.
    fn set_owned(&self, label: &str, value: String, window: &mut Window, cx: &mut App) {
        let Some(field) = self.fields.iter().find(|field| field.label == label) else {
            return;
        };
        field
            .state
            .update(cx, |state, cx| state.set_value(value, window, cx));
    }

    fn set(&self, label: &str, value: &'static str, window: &mut Window, cx: &mut App) {
        let Some(field) = self.fields.iter().find(|field| field.label == label) else {
            return;
        };
        field
            .state
            .update(cx, |state, cx| state.set_value(value, window, cx));
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

/// One browsing location, and everything that makes returning to it feel like
/// returning rather than starting again.
///
/// **Only the active tab's state is live.** The fields it mirrors sit directly
/// on [`Browser`]; switching tabs swaps a snapshot in and the outgoing one out.
/// The alternative — every one of the hundred and fifty reads of
/// `self.entries`, `self.prefix`, `self.selection` becoming `self.tab().…` — is
/// a refactor across the whole file, and it buys exactly one thing: inactive
/// tabs loading in the background. Which is LIST requests spent on a list
/// nobody is looking at.
pub struct Tab {
    id: u64,
    /// The snapshot. Empty for the tab that is currently live.
    state: TabState,
}

/// What a tab holds while it is not the one on screen.
#[derive(Default)]
pub struct TabState {
    bucket: Option<SharedString>,
    prefix: String,
    entries: Vec<Entry>,
    visible: Vec<usize>,
    continuation: Option<String>,
    sort: Sort,
    filter: String,
    selection: HashSet<String>,
    cursor: Option<String>,
    anchor: Option<String>,
    scroll: UniformListScrollHandle,
    layout: LayoutMode,
    search: Option<Search>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LayoutMode {
    #[default]
    List,
    Grid,
}

impl Tab {
    /// What the tab bar shows: the deepest name in the path, because that is
    /// what tells two tabs apart when they are in the same bucket.
    fn title(bucket: Option<&SharedString>, prefix: &str) -> SharedString {
        match bucket {
            None => "Trống".into(),
            Some(bucket) => match prefix.trim_end_matches('/').rsplit('/').next() {
                Some(segment) if !segment.is_empty() => segment.to_string().into(),
                _ => bucket.clone(),
            },
        }
    }
}

/// One operation applied to every object in a selection.
///
/// Driven a key at a time from the UI rather than looped inside one task: five
/// hundred objects is five hundred round trips, and a job that shows nothing
/// until it finishes cannot be judged or stopped. This is the same shape the
/// bucket scan uses, for the same reason.
pub struct Bulk {
    what: &'static str,
    keys: Vec<String>,
    done: usize,
    /// Keys that failed, with the reason. Collected rather than aborting on the
    /// first: one object with an ACL nobody may change should not stop the other
    /// four hundred and ninety nine.
    failed: Vec<String>,
    op: BulkOp,
    running: bool,
}

#[derive(Clone)]
pub enum BulkOp {
    Headers(s3core::ObjectHeaders),
    Acl(&'static str),
}

/// Fetching the rest of a prefix so that a sort can be exact.
///
/// **Why a sort needs this at all.** `ListObjectsV2` returns keys in
/// lexicographic order and offers no other. So sorting by name ascending is
/// free — it is the order the pages already arrive in — and every other sort is
/// a claim about keys that have not been fetched. Sorting by size over the
/// first thousand of twelve hundred keys answers "the largest of the first
/// thousand", which is not the question anyone asked, and nothing on screen
/// said so.
pub struct Completing {
    continuation: Option<String>,
    /// Pages land here rather than in `entries`, so the list on screen does not
    /// reshuffle under the pointer on every page. It is swapped in once, at the
    /// end, when the order is finally the right one.
    buffer: Vec<Entry>,
    requests: usize,
    running: bool,
    /// A cap stopped it before the end, so the sort is over a prefix of the
    /// prefix and has to say so.
    truncated: bool,
}

/// A scan of the whole bucket, running or finished.
///
/// Kept apart from the filter on purpose. The filter narrows what is already on
/// screen and costs nothing; this walks the entire keyspace and costs one LIST
/// request per thousand keys, which is a real charge on a real bill. So it only
/// starts when someone presses Enter, it says what it has spent, and it can be
/// stopped.
pub struct Search {
    query: String,
    /// Where the scan is. `None` before the first page and after the last.
    continuation: Option<String>,
    /// Keys looked at and requests made. Shown because the second one is what
    /// the provider charges for.
    scanned: usize,
    requests: usize,
    /// Whether the scan reached the end of the bucket. Until it does, a count
    /// of results is "so far" and must not be worded as though it were final.
    complete: bool,
    /// A cap stopped it. Different from being stopped by hand, and both are
    /// different from finishing: only one of the three means "this is the whole
    /// answer".
    capped: bool,
    /// Whether a request is in flight.
    running: bool,
}

/// What ⌘C or ⌘X put aside, waiting for a paste.
///
/// Holds keys rather than a live selection: the user is expected to navigate
/// somewhere else before pasting, and the selection is gone by then.
#[derive(Clone)]
pub struct Clipboard {
    bucket: SharedString,
    entries: Vec<Entry>,
    /// True for a cut: paste moves instead of copying.
    cut: bool,
}

#[derive(Debug)]
struct PerfStats {
    enabled: bool,
    renders: u64,
    last_render_at: Option<Instant>,
    frame_ms: u128,
    max_frame_ms: u128,
    visible: Option<(usize, usize)>,
    listing_ms: Option<u128>,
    listing_rows: usize,
    page_ms: Option<u128>,
    page_rows: usize,
    thumbnail_ms: Option<u128>,
    thumbnail_rows: usize,
}

impl Default for PerfStats {
    fn default() -> Self {
        Self {
            enabled: std::env::var_os("S3BROWSER_DEBUG").is_some(),
            renders: 0,
            last_render_at: None,
            frame_ms: 0,
            max_frame_ms: 0,
            visible: None,
            listing_ms: None,
            listing_rows: 0,
            page_ms: None,
            page_rows: 0,
            thumbnail_ms: None,
            thumbnail_rows: 0,
        }
    }
}

impl PerfStats {
    fn note_render(&mut self) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if let Some(previous) = self.last_render_at.replace(now) {
            self.frame_ms = now.duration_since(previous).as_millis();
            self.max_frame_ms = self.max_frame_ms.max(self.frame_ms);
        }
        self.renders += 1;
    }

    fn note_visible(&mut self, range: &Range<usize>) {
        if self.enabled {
            self.visible = Some((range.start, range.end));
        }
    }

    fn note_listing(&mut self, elapsed: Duration, rows: usize) {
        if self.enabled {
            self.listing_ms = Some(elapsed.as_millis());
            self.listing_rows = rows;
        }
    }

    fn note_page(&mut self, elapsed: Duration, rows: usize) {
        if self.enabled {
            self.page_ms = Some(elapsed.as_millis());
            self.page_rows = rows;
        }
    }

    fn note_thumbnails(&mut self, elapsed: Duration, rows: usize) {
        if self.enabled {
            self.thumbnail_ms = Some(elapsed.as_millis());
            self.thumbnail_rows = rows;
        }
    }

    fn label(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let visible = self
            .visible
            .map(|(start, end)| format!("{start}..{end}"))
            .unwrap_or_else(|| "-".into());
        let listing = self
            .listing_ms
            .map(|ms| format!("{ms}ms/{}", self.listing_rows))
            .unwrap_or_else(|| "-".into());
        let page = self
            .page_ms
            .map(|ms| format!("{ms}ms/{}", self.page_rows))
            .unwrap_or_else(|| "-".into());
        let thumbnails = self
            .thumbnail_ms
            .map(|ms| format!("{ms}ms/{}", self.thumbnail_rows))
            .unwrap_or_else(|| "-".into());
        Some(format!(
            "ui {}ms max {}ms · rows {visible} · list {listing} · page {page} · thumb {thumbnails}",
            self.frame_ms, self.max_frame_ms
        ))
    }
}

pub struct Browser {
    focus: FocusHandle,
    theme: Theme,
    chrome: Chrome,
    /// What the system last said. Kept because choosing "theo hệ thống" again
    /// has to repaint immediately, and the only other place that answer lives
    /// is inside a callback that has already run.
    appearance: gpui::WindowAppearance,
    /// Resolved once at startup: which of the candidate fonts this machine has.
    ui_font: SharedString,
    mono_font: SharedString,

    profiles: Vec<StoredProfile>,
    active_profile: Option<usize>,
    store: Option<ProfileStore>,
    /// Pinned and recently visited locations, kept on disk beside the profiles.
    places: Places,
    place_store: Option<PlaceStore>,
    settings: Settings,
    settings_store: Option<SettingsStore>,
    /// Whether the settings panel is open.
    settings_open: bool,
    /// Whether the About dialog is open.
    about_open: bool,

    client: Option<S3Client>,
    buckets: Vec<SharedString>,
    /// The bucket list per profile, with when it was fetched.
    ///
    /// `ListBuckets` is one request and one bill line every time a profile is
    /// switched to, and the answer changes about as often as someone creates a
    /// bucket — which is to say rarely. In memory rather than on disk: a stale
    /// list after a restart is a list nobody asked for, and the first request of
    /// a session has to happen anyway.
    bucket_cache: HashMap<String, (Vec<SharedString>, i64)>,
    /// When each bucket was created, as `ListBuckets` reported it.
    ///
    /// Keyed by name rather than held as rows, so the page always lists exactly
    /// what the sidebar lists: creating or deleting a bucket updates the name
    /// list from several places, and a second list of rows would drift out of
    /// step with it. A missing entry is a dash, not a gap in the table.
    bucket_created: HashMap<String, i64>,
    /// Whether what the sidebar shows came from that cache. Said out loud,
    /// because a bucket someone just made in the console not appearing is
    /// otherwise indistinguishable from the app being broken.
    buckets_cached: bool,
    /// Set for one connect, by the refresh control.
    bucket_cache_bypass: bool,
    /// Narrows the sidebar. Only exists once there are enough buckets for the
    /// list to be worth narrowing — see `BUCKET_FILTER_MIN`.
    bucket_filter: String,
    bucket_filter_input: Option<Entity<InputState>>,
    bucket: Option<SharedString>,
    prefix: String,

    /// Everything fetched so far for the current prefix, already sorted.
    entries: Vec<Entry>,
    /// Indices into `entries` that survive the filter — what the list renders.
    visible: Vec<usize>,
    continuation: Option<String>,
    loading: bool,
    loading_more: bool,
    /// Whether a connection is being made. Separate from `loading`: the sidebar
    /// and the object list wait on different requests, and one shared flag
    /// cannot say which of them is the one still busy.
    connecting: bool,
    /// Bumped on every navigation so a late response for an abandoned prefix is
    /// dropped instead of overwriting the current listing.
    generation: u64,

    sort: Sort,
    /// What the filter field holds, mirrored here so the list can be narrowed
    /// without reading the widget on every frame.
    filter: String,
    /// The `s3://` box in the title bar. Always on screen, after Brows3: it is
    /// the only way in when the token cannot list buckets, and a box that is
    /// always there also always says where you are.
    path_input: Option<Entity<InputState>>,
    /// The box needs rewriting to match where we now are. A flag rather than a
    /// direct write, because writing into a text field needs a `Window` and
    /// most of what navigates — a task finishing, a click deep in a listener —
    /// does not have one. `render` does, and runs right after.
    path_dirty: bool,
    /// The filter field itself. A real input rather than a key-capture bar: it
    /// is on screen all the time now, and a permanent field that cannot take a
    /// paste or a cursor key is a worse lie than no field at all.
    ///
    /// Optional because building an `InputState` needs a `Window`, and the view
    /// tests deliberately run without one. Everything that touches it copes
    /// with `None` rather than the tests giving up on covering this file.
    filter_input: Option<Entity<InputState>>,
    selection: HashSet<String>,
    /// Which row the keyboard is sitting on, held as an object key rather than
    /// a row number: filtering and sorting renumber the rows, and a cursor that
    /// silently slides onto a different file when the filter changes is worse
    /// than no cursor at all.
    cursor: Option<String>,
    /// The row currently under the pointer. Inline row actions are hidden until
    /// they are useful; keeping the hover here lets them reveal with the same
    /// motion language as the rest of the app instead of popping in as a style
    /// override.
    hovered_row: Option<usize>,
    /// Where a Shift-range started. A key too, and for the same reason.
    anchor: Option<String>,
    /// Object pane presentation: dense list for operations, grid for scanning
    /// folders and image-heavy prefixes.
    layout: LayoutMode,

    scroll: UniformListScrollHandle,
    status: SharedString,
    /// Everything that has gone wrong, newest last.
    ///
    /// A list rather than one slot: the old single `error` was overwritten by
    /// whatever failed next, so a burst of failures during a batch delete left
    /// exactly one of them on screen and no way to reach the rest. Kept until
    /// dismissed, because an error that clears itself while it is being read is
    /// an error nobody reads.
    failures: Vec<Failure>,
    /// Whether the failure log is open.
    failures_open: bool,
    perf: PerfStats,

    transfers: TransferEngine,
    drawer_open: bool,
    /// Folder rows in the drawer the user has opened, by group key.
    ///
    /// Kept on the view rather than on the job, because the queue is rebuilt
    /// from a snapshot every frame: state stored beside a job would be thrown
    /// away and the folder would snap shut while it was being read.
    expanded_folders: HashSet<String>,

    confirm: Option<Confirm>,
    /// An SSO sign-in in progress, and the roles it turned up once done.
    sso: Option<SsoFlow>,
    /// The add-profile form, when open.
    form: Option<Form>,
    clipboard: Option<Clipboard>,
    /// Decoded thumbnails, keyed by object key. `None` marks a key already
    /// tried and rejected, so a failure is not retried on every repaint.
    thumbnails: HashMap<String, Option<std::sync::Arc<gpui::Image>>>,
    /// Whether the profile manager dialog is open.
    profiles_open: bool,
    /// Every open tab, in the order the bar shows them. Never empty: closing the
    /// last one would leave the window with nowhere to be.
    tabs: Vec<Tab>,
    /// Which tab's state is the live one.
    active_tab: usize,
    next_tab_id: u64,
    /// The preview overlay, when one is open.
    previewing: Option<Previewing>,
    /// A file on its way down so another app can open it.
    opening: Option<Opening>,
    screen: Screen,
    /// Whether the path box is showing where you have been.
    ///
    /// Driven by the field's own focus events rather than by whatever opened
    /// it, so clicking into the box gets the same list that ⌘L does.
    path_suggesting: bool,
    /// Which suggestion the arrow keys are on. `None` means "none of them" —
    /// Enter then goes to whatever is typed, which is what the box is for.
    path_choice: Option<usize>,
    preview_task: Option<Task<()>>,
    /// Loading the rest of a prefix so the current sort is exact.
    completing: Option<Completing>,
    /// An operation running over a selection.
    bulk: Option<Bulk>,
    /// A whole-bucket scan, when one is running or its results are on screen.
    /// While this is `Some`, `entries` holds results rather than one prefix.
    search: Option<Search>,
    /// The command palette: `Some` with the query typed so far.
    palette: Option<String>,
    /// Which row the palette has highlighted.
    palette_selected: usize,
    /// The palette's own scroll handle. Same mechanism as the object list, so
    /// arrow keys scroll the highlighted row into view in both places.
    palette_scroll: UniformListScrollHandle,
    /// The share panel: which key it is for, and the URL once it exists.
    share: Option<Share>,
    inspector: Option<Inspection>,
    acl_popup_open: bool,
    /// Whether the open bucket keeps versions, so a delete confirmation can say
    /// whether it removes data or only hides it. Refreshed when the bucket
    /// changes, not on every navigation within one.
    ///
    /// Distinct from the capability below: this is whether versioning is turned
    /// on, that is whether the provider implements it at all.
    bucket_versioned: bool,
    /// What the provider can do with the open bucket, so the UI can leave out
    /// what it cannot rather than offering a button that returns 501.
    capabilities: Option<Capabilities>,
    caps_cache: CapabilityCache,
    /// True while a repaint loop is running to animate transfer progress.
    ticking: bool,

    /// Named slots rather than a growing vec: replacing the listing task cancels
    /// the request it superseded, and nothing accumulates for the session.
    connect_task: Option<Task<()>>,
    listing_task: Option<Task<()>>,
    paging_task: Option<Task<()>>,
    op_task: Option<Task<()>>,
    /// The handoff fetch's own slot. It used to share `op_task`, which meant any
    /// other operation cancelled the download and left nothing to say so.
    open_task: Option<Task<()>>,
    /// Capability probing gets its own slot: it runs alongside whatever the user
    /// is doing, and sharing `op_task` meant opening the inspector cancelled it.
    caps_task: Option<Task<()>>,
    /// Thumbnails load in the background and must not cancel a user action, so
    /// they get their own slot rather than sharing `op_task`.
    thumb_task: Option<Task<()>>,
    tick_task: Option<Task<()>>,
    /// The scan's own slot, so stopping a search is dropping one task and not
    /// cancelling whatever else was in flight.
    search_task: Option<Task<()>>,
    /// Likewise for a bulk edit, which outlives several other operations.
    bulk_task: Option<Task<()>>,
    complete_task: Option<Task<()>>,
    _appearance: Option<Subscription>,
    /// Kept alive, not read: dropping it would silently stop the filter from
    /// reacting to what is typed into it.
    _filter_events: Option<Subscription>,
    _bucket_filter_events: Option<Subscription>,
    _path_events: Option<Subscription>,
}

impl Browser {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let chrome = Chrome::detect();

        // The UI font ships with the binary, so there is nothing to probe for:
        // `all_font_names` lists what the *system* has installed and does not
        // include fonts registered through `add_fonts`, so asking would fall
        // through to a system font and quietly discard the bundled one.
        let ui_font = SharedString::from(platform::BUNDLED_UI_FONT);
        // Monospace is not bundled, so this one is a real choice among whatever
        // the machine happens to have.
        let mono_font = SharedString::from(platform::pick_font(
            platform::mono_font_candidates(),
            &cx.text_system().all_font_names(),
        ));
        debug_log!("font: {ui_font} / {mono_font}");

        // Follow the system between light and dark without a restart — unless
        // the settings say otherwise, in which case a system change is not this
        // app's business.
        let weak = cx.entity().downgrade();
        let appearance = window.observe_window_appearance(move |window, cx| {
            let appearance = window.appearance();
            _ = weak.update(cx, |this: &mut Self, cx| {
                this.appearance = appearance;
                this.theme = theme_for(this.settings.theme, appearance, this.chrome);
                cx.notify();
            });
        });

        let path_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("s3://bucket/prefix/"));
        let path_events = cx.subscribe_in(
            &path_input,
            window,
            |this: &mut Self, state, event: &InputEvent, window, cx| match event {
                // Focus rather than whatever opened the box: clicking into it
                // has to offer the same list ⌘L does.
                InputEvent::Focus => {
                    this.path_suggesting = true;
                    this.path_choice = None;
                    cx.notify();
                }
                InputEvent::Blur => {
                    this.path_suggesting = false;
                    cx.notify();
                }
                InputEvent::Change => {
                    // The highlighted row belonged to the old list; keeping the
                    // index would point it at a different place.
                    this.path_choice = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    // A highlighted suggestion wins over the text, because the
                    // text is the *old* text — arrowing down does not rewrite
                    // the box, so what is typed is not what is chosen.
                    let chosen = this
                        .path_choice
                        .and_then(|index| this.path_suggestions(cx).get(index).cloned());
                    match chosen {
                        Some(place) => this.open_place(&place, cx),
                        None => {
                            let text = state.read(cx).value().to_string();
                            this.go_to_path(&text, cx);
                        }
                    }
                    this.path_suggesting = false;
                    this.path_choice = None;
                    // Back to the list, so the arrow keys work again without a
                    // trip to the mouse.
                    this.focus.focus(window);
                    cx.notify();
                }
            },
        );

        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("Lọc theo tên"));
        // Live rather than on Enter: the list is right there, and making someone
        // commit a filter to find out whether it matched anything is a round
        // trip through their own attention for no reason.
        let filter_events = cx.subscribe(
            &filter_input,
            |this: &mut Self, state, event: &InputEvent, cx| {
                match event {
                    InputEvent::Change => {
                        this.filter = state.read(cx).value().to_string();
                        this.resort_and_filter();
                        this.status = this.search_summary();
                        cx.notify();
                    }
                    // Enter is the opt-in. Filtering what is already loaded is
                    // free and happens as you type; scanning the whole bucket
                    // costs a LIST request per thousand keys, so it waits to be
                    // asked for.
                    InputEvent::PressEnter { .. } => {
                        let query = state.read(cx).value().to_string();
                        this.start_search(query, cx);
                    }
                    _ => {}
                }
            },
        );

        let bucket_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Tìm bucket"));
        // No Enter handling here on purpose: every bucket name is already in
        // memory, so there is nothing to go and fetch and nothing to charge for.
        let bucket_filter_events = cx.subscribe(
            &bucket_filter_input,
            |this: &mut Self, state, event: &InputEvent, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                this.bucket_filter = state.read(cx).value().to_string();
                cx.notify();
            },
        );

        let store = ProfileStore::default_location().ok();
        let place_store = store.as_ref().map(|store| PlaceStore::beside(store.path()));
        let settings_store = store
            .as_ref()
            .map(|store| SettingsStore::beside(store.path()));
        let settings = settings_store
            .as_ref()
            .map(|store| store.load())
            .unwrap_or_default();
        set_reduce_motion(motion_choice_reduces(settings.motion));
        let places = place_store
            .as_ref()
            .map(|store| store.load())
            .unwrap_or_default();
        let profiles = store
            .as_ref()
            .and_then(|store| store.load().ok())
            .unwrap_or_default();

        // The queue lives next to the profiles so unfinished transfers come back
        // after a restart.
        let transfers = store
            .as_ref()
            .and_then(|store| store.path().parent().map(|dir| dir.join("transfers.db")))
            .and_then(|path| TransferEngine::open_with(&path, settings.job_concurrency()).ok())
            .or_else(|| TransferEngine::in_memory().ok())
            .expect("an in-memory queue always opens");
        transfers.set_bandwidth_limit(settings.bandwidth_limit);

        // Explicit, because nothing else does it: with no focused handle the
        // window has nowhere to send a key press, so the shortcuts and the
        // arrow keys are dead until something is clicked. It used to survive on
        // whatever gpui focused by default, which stopped being the list the
        // moment a real text field appeared in the toolbar.
        let focus = cx.focus_handle();
        window.focus(&focus);

        let mut this = Self {
            focus,
            theme: theme_for(settings.theme, window.appearance(), chrome),
            chrome,
            appearance: window.appearance(),
            ui_font: ui_font.clone(),
            mono_font,
            profiles,
            active_profile: None,
            store,
            places,
            place_store,
            settings: settings.clone(),
            settings_store,
            settings_open: false,
            about_open: false,
            client: None,
            buckets: Vec::new(),
            bucket_cache: HashMap::new(),
            bucket_created: HashMap::new(),
            buckets_cached: false,
            bucket_cache_bypass: false,
            bucket_filter: String::new(),
            bucket_filter_input: Some(bucket_filter_input),
            bucket: None,
            prefix: String::new(),
            entries: Vec::new(),
            visible: Vec::new(),
            continuation: None,
            loading: false,
            loading_more: false,
            connecting: false,
            generation: 0,
            sort: Sort::default(),
            filter: String::new(),
            path_input: Some(path_input),
            path_dirty: false,
            filter_input: Some(filter_input),
            selection: HashSet::new(),
            cursor: None,
            hovered_row: None,
            anchor: None,
            layout: LayoutMode::List,
            scroll: UniformListScrollHandle::new(),
            status: "Chọn một profile để bắt đầu".into(),
            failures: Vec::new(),
            failures_open: false,
            perf: PerfStats::default(),
            transfers,
            drawer_open: false,
            expanded_folders: HashSet::new(),

            confirm: None,
            form: None,
            clipboard: None,
            thumbnails: HashMap::new(),
            profiles_open: false,
            sso: None,
            tabs: vec![Tab {
                id: 0,
                state: TabState::default(),
            }],
            active_tab: 0,
            next_tab_id: 1,
            previewing: None,
            opening: None,
            screen: Screen::default(),
            path_suggesting: false,
            path_choice: None,
            preview_task: None,
            completing: None,
            bulk: None,
            search: None,
            palette: None,
            palette_selected: 0,
            palette_scroll: UniformListScrollHandle::new(),
            share: None,
            inspector: None,
            acl_popup_open: false,
            bucket_versioned: false,
            capabilities: None,
            caps_cache: CapabilityCache::default(),
            ticking: false,
            connect_task: None,
            listing_task: None,
            paging_task: None,
            op_task: None,
            open_task: None,
            caps_task: None,
            thumb_task: None,
            tick_task: None,
            search_task: None,
            bulk_task: None,
            complete_task: None,
            _appearance: Some(appearance),
            _filter_events: Some(filter_events),
            _bucket_filter_events: Some(bucket_filter_events),
            _path_events: Some(path_events),
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
            self.fail(Failure::known(
                "Không lưu được danh sách profile",
                format!("{}: {error}", store.path().display()),
                None,
            ));
        }
    }

    fn add_profile(&mut self, profile: StoredProfile, secret: &str, cx: &mut Context<Self>) {
        if let Err(error) = vault::set_secret_key(&profile.id, secret) {
            self.fail(Failure::known(
                "Không lưu được khoá bí mật vào chuỗi khoá",
                format!("{error}"),
                None,
            ));
            return;
        }
        self.profiles.push(profile);
        self.save_profiles();
        self.connect(self.profiles.len() - 1, cx);
    }

    fn update_profile(
        &mut self,
        index: usize,
        profile: StoredProfile,
        secret: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if index >= self.profiles.len() {
            self.report("Profile không còn tồn tại".into());
            return;
        }
        if let Some(secret) = secret.as_deref() {
            if let Err(error) = vault::set_secret_key(&profile.id, secret) {
                self.fail(Failure::known(
                    "Không lưu được khoá bí mật vào chuỗi khoá",
                    format!("{error}"),
                    None,
                ));
                return;
            }
        }

        let active = self.active_profile == Some(index);
        let profile_id = profile.id.clone();
        self.profiles[index] = profile;
        self.bucket_cache.remove(&profile_id);
        self.save_profiles();

        if active {
            self.connect(index, cx);
        } else {
            self.status = "Đã lưu profile".into();
            cx.notify();
        }
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

    fn connect(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(stored) = self.profiles.get(index).cloned() else {
            return;
        };

        let secret = match vault::secret_key(&stored.id) {
            Ok(secret) => secret,
            Err(error) => {
                self.fail(Failure::known(
                    "Không đọc được khoá bí mật từ chuỗi khoá",
                    format!("{error}"),
                    Some(Fix::EditProfile),
                ));
                cx.notify();
                return;
            }
        };

        // A list fetched minutes ago is the same list, and the request is not
        // free. The refresh control beside the header forces past it.
        let cached = self
            .bucket_cache
            .get(&stored.id)
            .filter(|(_, at)| {
                !self.bucket_cache_bypass && s3core::now_epoch() - at < BUCKET_CACHE_TTL
            })
            .map(|(buckets, _)| buckets.clone());
        self.bucket_cache_bypass = false;

        self.active_profile = Some(index);
        self.buckets_cached = cached.is_some();
        self.client = None;
        self.buckets.clear();
        self.bucket = None;
        self.entries.clear();
        self.visible.clear();
        self.connecting = true;
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

        let profile_id = stored.id.clone();

        let connecting = Tokio::spawn(cx, async move {
            let client = S3Client::connect(&profile).await?;
            // Not `?`: a token scoped to one bucket — the normal, recommended
            // setup on R2 — is denied ListBuckets while the bucket itself works
            // fine. Failing the whole connection here left those users with no
            // way in at all.
            let buckets = match cached {
                Some(cached) => {
                    debug_log!("buckets from cache: {}", cached.len());
                    Ok(cached
                        .iter()
                        .map(|name| s3core::BucketInfo {
                            name: name.to_string(),
                            // The cache holds names only. The date is filled in
                            // again on the next real fetch; a page showing dashes
                            // for a few minutes beats a request nobody asked for.
                            created: None,
                        })
                        .collect())
                }
                None => {
                    debug_log!("buckets: asking the provider");
                    client.list_buckets_detailed().await
                }
            };
            anyhow::Ok((client, buckets))
        });

        let task = cx.spawn(async move |this, cx| {
            let outcome = connecting.await;
            _ = this.update(cx, |this, cx| {
                this.connecting = false;
                match outcome {
                    Ok(Ok((client, listed))) => {
                        let listed = match listed {
                            Ok(buckets) => {
                                this.status = SharedString::default();
                                this.bucket_cache.insert(
                                    profile_id.clone(),
                                    (
                                        buckets
                                            .iter()
                                            .map(|bucket| SharedString::from(bucket.name.clone()))
                                            .collect(),
                                        // Not refreshed on a cache hit: the age
                                        // shown has to be the age of the answer,
                                        // not of the last time it was reused.
                                        this.bucket_cache
                                            .get(&profile_id)
                                            .map(|(_, at)| *at)
                                            .filter(|_| this.buckets_cached)
                                            .unwrap_or_else(s3core::now_epoch),
                                    ),
                                );
                                Some(buckets)
                            }
                            Err(error) => {
                                debug_log!("ListBuckets failed: {error}");
                                // The request name goes into the text so the
                                // classifier can reach the R2 case: a bare 403
                                // says nothing, `ListBuckets` plus a 403 says
                                // "bucket-scoped token", which is the setup
                                // R2's own docs recommend.
                                //
                                // `or_fix` and not a fixed answer, because this
                                // same call site also sees expired keys and
                                // dead networks, and for those the classifier
                                // names the real cause. Opening a bucket by
                                // name is only the fallback.
                                this.fail(
                                    Failure::new(format!("ListBuckets: {error:?}"))
                                        .or_fix(Fix::OpenBucketByName),
                                );
                                None
                            }
                        };
                        if let Some(buckets) = listed {
                            debug_log!("connected: {} buckets", buckets.len());
                            this.remember_bucket_dates(&buckets);
                            this.buckets = buckets
                                .into_iter()
                                .map(|bucket| SharedString::from(bucket.name))
                                .collect();
                        } else {
                            debug_log!("connected: bucket list unavailable");
                            this.status = "Nhập bucket bạn có quyền để mở trực tiếp".into();
                        }
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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        });
        self.connect_task = Some(task);
        cx.notify();
    }

    /// Classifies a raw provider error and records it.
    ///
    /// Callers pass `{error:?}` rather than `{error}` on purpose. `anyhow`'s
    /// `Display` prints only the outermost context, so `.context("ListBuckets
    /// failed")` throws away the provider's own words — the very text the
    /// classifier reads and the only text worth pasting into a support ticket.
    /// `Debug` keeps the whole chain.
    fn report(&mut self, message: String) {
        self.fail(Failure::new(message));
    }

    /// Records a failure. Also to stderr, so someone running from a terminal
    /// has a copy that outlives the window.
    fn fail(&mut self, failure: Failure) {
        eprintln!("[s3browser] error: {}", failure.detail);
        self.failures.push(failure);
        // A cap, because a retry loop against a dead endpoint would otherwise
        // grow this without limit. The oldest go first: the newest failure is
        // the one being looked at.
        if self.failures.len() > FAILURE_LIMIT {
            let excess = self.failures.len() - FAILURE_LIMIT;
            self.failures.drain(..excess);
        }
    }

    /// Runs the one repair a failure offers.
    fn apply_fix(&mut self, fix: Fix, window: &mut Window, cx: &mut Context<Self>) {
        self.failures_open = false;
        match fix {
            Fix::OpenBucketByName => self.open_form(FormKind::OpenBucket, window, cx),
            Fix::EditProfile => {
                if let Some(index) = self.active_profile {
                    self.profiles_open = false;
                    self.open_form(FormKind::EditProfile(index), window, cx);
                } else {
                    self.profiles_open = true;
                    cx.notify();
                }
            }
            Fix::Retry => match (self.bucket.clone(), self.prefix.clone()) {
                (Some(bucket), prefix) => self.open(bucket, prefix, cx),
                // Nothing open yet, so the thing to retry is the connection.
                (None, _) => {
                    if let Some(index) = self.active_profile {
                        self.connect(index, cx);
                    }
                }
            },
        }
    }

    // -------------------------------------------------------------- navigation

    pub fn open(&mut self, bucket: SharedString, prefix: String, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };

        self.generation += 1;
        let generation = self.generation;
        // Opening a prefix means the list is a listing again. The scan is
        // already abandoned by the generation bump; this is what stops the
        // strip and the counts from outliving it.
        self.search = None;
        self.search_task = None;
        self.completing = None;
        self.complete_task = None;

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
        self.path_dirty = true;
        self.entries.clear();
        self.visible.clear();
        self.selection.clear();
        self.cursor = None;
        self.anchor = None;
        self.continuation = None;
        self.loading = true;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);

        let started = Instant::now();
        let listing = Tokio::spawn(
            cx,
            async move { client.list_page(&bucket, &prefix, None).await },
        );

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
                        this.perf
                            .note_listing(started.elapsed(), page.entries.len());
                        debug_log!(
                            "listed {} entries in {}ms, more={}",
                            page.entries.len(),
                            started.elapsed().as_millis(),
                            page.continuation.is_some()
                        );
                        this.entries = page.entries;
                        this.continuation = page.continuation;
                        this.resort_and_filter();
                        // Recorded on arrival, not on request: a prefix that
                        // fails to open is not somewhere anyone has been.
                        this.record_visit();
                        this.status = this.listing_summary();
                    }
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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

        let started = Instant::now();
        let listing = Tokio::spawn(cx, async move {
            client.list_page(&bucket, &prefix, Some(token)).await
        });

        let task = cx.spawn(async move |this, cx| {
            // Repaint before waiting, not during. Paging is decided inside the
            // list's own processor, which runs after this frame's element tree
            // was already built, and a `notify` from there is swallowed — the
            // frame doing the rendering is not going to render itself again. So
            // the request for a repaint has to be made from outside the frame,
            // and the first thing this task does is step outside it. Without
            // this the "loading more" line appears only once the page it
            // announces has already arrived, which is to say never.
            _ = this.update(cx, |_this, cx| cx.notify());

            let outcome = listing.await;
            _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading_more = false;
                match outcome {
                    Ok(Ok(page)) => {
                        this.perf.note_page(started.elapsed(), page.entries.len());
                        debug_log!(
                            "page +{} entries in {}ms",
                            page.entries.len(),
                            started.elapsed().as_millis()
                        );
                        this.entries.extend(page.entries);
                        this.continuation = page.continuation;
                        this.resort_and_filter();
                        this.status = this.listing_summary();
                    }
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        });
        self.paging_task = Some(task);
    }

    fn listing_summary(&self) -> SharedString {
        if let Some(completing) = self.completing.as_ref() {
            return format!(
                "Đang tải nốt để sắp xếp: {} mục, {} yêu cầu",
                self.entries.len() + completing.buffer.len(),
                completing.requests
            )
            .into();
        }

        // The whole point of the completing machinery: when the prefix is not
        // all here, a sort by size or date is an answer about the part that is,
        // and staying quiet about that is the bug.
        if needs_complete_listing(self.sort) && self.continuation.is_some() {
            return format!("{} mục, sắp xếp trên phần đã tải", self.visible.len()).into();
        }
        // Otherwise nothing: the count moved to the footer under the list, and
        // the status bar saying it a second time is a line of chrome spent
        // repeating the line above it.
        SharedString::default()
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

    // ------------------------------------------------------------------- tabs

    /// Lifts the live state out into a snapshot.
    ///
    /// Moves rather than clones: a thousand `Entry` values copied on every tab
    /// switch is a cost for nothing, and the live fields are about to be
    /// overwritten anyway.
    fn capture_tab(&mut self) -> TabState {
        TabState {
            bucket: self.bucket.take(),
            prefix: std::mem::take(&mut self.prefix),
            entries: std::mem::take(&mut self.entries),
            visible: std::mem::take(&mut self.visible),
            continuation: self.continuation.take(),
            sort: self.sort,
            filter: std::mem::take(&mut self.filter),
            selection: std::mem::take(&mut self.selection),
            cursor: self.cursor.take(),
            anchor: self.anchor.take(),
            scroll: self.scroll.clone(),
            layout: self.layout,
            search: self.search.take(),
        }
    }

    /// The half of restoring that is only moving fields, split out so it can be
    /// tested without a window.
    fn apply_tab_state(&mut self, state: TabState) {
        self.bucket = state.bucket;
        self.prefix = state.prefix;
        self.entries = state.entries;
        self.visible = state.visible;
        self.continuation = state.continuation;
        self.sort = state.sort;
        self.filter = state.filter;
        self.selection = state.selection;
        self.cursor = state.cursor;
        self.anchor = state.anchor;
        self.scroll = state.scroll;
        self.layout = state.layout;
        self.search = state.search;
    }

    fn restore_tab(&mut self, state: TabState, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_tab_state(state);
        self.path_dirty = true;

        // The filter field is one widget shared by every tab, so its text has to
        // be put back by hand or the box would still show the last tab's filter
        // while this tab's list is narrowed by a different one.
        if let Some(input) = self.filter_input.clone() {
            let filter = self.filter.clone();
            input.update(cx, |input, cx| input.set_value(filter, window, cx));
        }

        // Whatever was in flight belongs to the tab being left.
        self.loading = false;
        self.loading_more = false;
        self.listing_task = None;
        self.paging_task = None;
        self.completing = None;
        self.complete_task = None;

        // A tab that has a place but nothing in it has never been loaded, or was
        // switched away from mid-load. Either way it needs the request now; a
        // tab that already has rows must not spend one.
        if self.entries.is_empty() {
            if let Some(bucket) = self.bucket.clone() {
                let prefix = self.prefix.clone();
                self.open(bucket, prefix, cx);
            }
        } else {
            self.status = self.listing_summary();
        }
        cx.notify();
    }

    fn switch_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index == self.active_tab || index >= self.tabs.len() {
            return;
        }
        // Abandons any request still running for the tab being left, so its
        // answer cannot land in the new tab's list.
        self.generation += 1;

        let state = self.capture_tab();
        self.tabs[self.active_tab].state = state;
        self.active_tab = index;
        let state = std::mem::take(&mut self.tabs[index].state);
        self.restore_tab(state, window, cx);
    }

    /// Opens a location in a new tab, or jumps to the tab already showing it.
    fn open_in_tab(
        &mut self,
        bucket: Option<SharedString>,
        prefix: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Same place twice is two tabs that can never be told apart in the bar.
        let existing = self.tabs.iter().position(|tab| {
            if tab.id == self.tabs[self.active_tab].id {
                self.bucket == bucket && self.prefix == prefix
            } else {
                tab.state.bucket == bucket && tab.state.prefix == prefix
            }
        });
        if let Some(index) = existing {
            return self.switch_tab(index, window, cx);
        }

        self.generation += 1;
        let state = self.capture_tab();
        self.tabs[self.active_tab].state = state;

        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.tabs.insert(
            self.active_tab + 1,
            Tab {
                id,
                state: TabState {
                    bucket,
                    prefix,
                    sort: self.sort,
                    layout: self.layout,
                    ..Default::default()
                },
            },
        );
        self.active_tab += 1;
        let state = std::mem::take(&mut self.tabs[self.active_tab].state);
        self.restore_tab(state, window, cx);
    }

    /// A new tab at the root of whatever bucket is open, or empty if none is.
    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Not a duplicate of the current place: `open_in_tab` would jump back to
        // this tab if the prefix happened to be the root already.
        let bucket = self.bucket.clone();
        if bucket.is_some() && self.prefix.is_empty() {
            return self.open_in_tab(None, String::new(), window, cx);
        }
        self.open_in_tab(bucket, String::new(), window, cx)
    }

    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // The last tab stays. A window with no tab has nowhere to be, and an
        // empty one is what "close everything" should leave behind anyway.
        if self.tabs.len() <= 1 {
            return;
        }
        if index != self.active_tab {
            self.tabs.remove(index);
            if index < self.active_tab {
                self.active_tab -= 1;
            }
            return cx.notify();
        }

        self.generation += 1;
        self.tabs.remove(index);
        // The one to the right, or the last one if this was the rightmost —
        // what every browser does, and what the hand expects.
        self.active_tab = index.min(self.tabs.len() - 1);
        let state = std::mem::take(&mut self.tabs[self.active_tab].state);
        self.restore_tab(state, window, cx);
    }

    /// Opens whatever the cursor is on in a new tab.
    ///
    /// Only folders: a tab is a place, and an object is not one.
    fn open_cursor_in_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .cursor_position()
            .and_then(|position| self.visible.get(position).copied())
            .and_then(|index| self.entries.get(index))
            .filter(|entry| entry.is_folder)
            .map(|entry| entry.key.clone());

        if let (Some(bucket), Some(prefix)) = (self.bucket.clone(), target) {
            self.open_in_tab(Some(bucket), prefix, window, cx);
        }
    }

    /// The label for each tab, taken from live state for the active one.
    fn tab_titles(&self) -> Vec<SharedString> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                if index == self.active_tab {
                    Tab::title(self.bucket.as_ref(), &self.prefix)
                } else {
                    Tab::title(tab.state.bucket.as_ref(), &tab.state.prefix)
                }
            })
            .collect()
    }

    // ------------------------------------------------------------ bulk edits

    /// The selected objects, in the order they appear on screen.
    ///
    /// Folders are left out on purpose: a folder is a common prefix, not an
    /// object, and copying one onto itself to rewrite its headers would ask the
    /// provider about a key that does not exist.
    fn selected_object_keys(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| !entry.is_folder && self.selection.contains(&entry.key))
            .map(|entry| entry.key.clone())
            .collect()
    }

    fn inspection_key_for_selection(&self) -> Option<String> {
        self.cursor
            .as_ref()
            .filter(|key| self.selection.contains(*key))
            .and_then(|key| {
                self.entries
                    .iter()
                    .find(|entry| entry.key == *key && !entry.is_folder)
                    .map(|entry| entry.key.clone())
            })
            .or_else(|| self.selected_object_keys().first().cloned())
    }

    /// Starts an operation over `keys`.
    fn start_bulk(
        &mut self,
        what: &'static str,
        keys: Vec<String>,
        op: BulkOp,
        cx: &mut Context<Self>,
    ) {
        if keys.is_empty() || self.client.is_none() || self.bucket.is_none() {
            return;
        }
        if matches!(&op, BulkOp::Acl(_)) {
            if let Some(failure) = self.acl_unavailable_failure() {
                self.fail(failure);
                cx.notify();
                return;
            }
        }
        self.bulk = Some(Bulk {
            what,
            keys,
            done: 0,
            failed: Vec::new(),
            op,
            running: true,
        });
        self.status = self.bulk_summary();
        self.bulk_step(cx);
        cx.notify();
    }

    /// Does one key, then queues the next.
    fn bulk_step(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(bulk)) =
            (self.client.clone(), self.bucket.clone(), self.bulk.as_ref())
        else {
            return;
        };
        if !bulk.running {
            return;
        }
        let Some(key) = bulk.keys.get(bulk.done).cloned() else {
            // Nothing left. Say how it went, once, rather than per key.
            return self.finish_bulk(cx);
        };

        let op = bulk.op.clone();
        let op_is_acl = matches!(op, BulkOp::Acl(_));
        let running = Tokio::spawn(cx, async move {
            match op {
                BulkOp::Headers(headers) => {
                    client.set_object_headers(&bucket, &key, &headers).await
                }
                BulkOp::Acl(canned) => client.set_object_acl(&bucket, &key, canned).await,
            }
        });

        self.bulk_task = Some(cx.spawn(async move |this, cx| {
            let outcome = running.await;
            _ = this.update(cx, |this, cx| {
                let mut acl_not_supported = false;
                {
                    let Some(bulk) = this.bulk.as_mut() else {
                        return;
                    };
                    let key = bulk.keys[bulk.done].clone();
                    bulk.done += 1;
                    match outcome {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            let detail = format!("{error:?}");
                            if op_is_acl && detail.contains("AccessControlListNotSupported") {
                                acl_not_supported = true;
                            }
                            bulk.failed.push(format!("{key}: {detail}"));
                        }
                        Err(error) => bulk.failed.push(format!("{key}: task lỗi: {error}")),
                    }
                }
                if acl_not_supported {
                    this.mark_acl_unsupported();
                }
                this.status = this.bulk_summary();
                this.bulk_step(cx);
                cx.notify();
            });
        }));
    }

    fn finish_bulk(&mut self, cx: &mut Context<Self>) {
        let Some(bulk) = self.bulk.take() else {
            return;
        };
        self.bulk_task = None;

        if bulk.failed.is_empty() {
            self.status = format!("{} xong {} mục", bulk.what, bulk.done).into();
        } else {
            // One summary with every reason under it, not one red line per key:
            // the log holds more than one failure precisely so a batch can
            // report as a batch.
            self.fail(Failure::known(
                &format!(
                    "{} được {}/{} mục",
                    bulk.what,
                    bulk.done - bulk.failed.len(),
                    bulk.keys.len()
                ),
                bulk.failed.join("\n"),
                None,
            ));
        }
        // The panel is showing what the object used to be.
        if let Some(inspector) = self.inspector.as_ref() {
            let key = inspector.key.clone();
            self.load_inspection(key, cx);
        }
        cx.notify();
    }

    fn stop_bulk(&mut self, cx: &mut Context<Self>) {
        if let Some(bulk) = self.bulk.as_mut() {
            bulk.running = false;
        }
        self.bulk_task = None;
        self.finish_bulk(cx);
    }

    fn bulk_summary(&self) -> SharedString {
        match self.bulk.as_ref() {
            Some(bulk) => format!("{} {}/{}…", bulk.what, bulk.done, bulk.keys.len()).into(),
            None => self.search_summary(),
        }
    }

    // ------------------------------------------------------------- searching

    /// Starts a scan of the whole bucket for `query`.
    ///
    /// Results land in `entries`, the same place a listing does, so selecting,
    /// downloading, inspecting and the context menu all keep working without
    /// knowing a search happened. They act on `entry.key`, which is the real
    /// key wherever it came from.
    fn start_search(&mut self, query: String, cx: &mut Context<Self>) {
        let query = query.trim().to_string();
        if query.is_empty() || self.bucket.is_none() || self.client.is_none() {
            return;
        }

        // Shares the listing's generation counter deliberately: navigating away
        // must abandon a scan, and a scan must abandon a listing. Two counters
        // would let one of them write into the other's results.
        self.generation += 1;
        self.entries.clear();
        self.visible.clear();
        self.selection.clear();
        self.cursor = None;
        self.anchor = None;
        self.continuation = None;
        self.loading = false;
        self.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);

        self.search = Some(Search {
            query,
            continuation: None,
            scanned: 0,
            requests: 0,
            complete: false,
            capped: false,
            running: true,
        });
        self.status = self.search_summary();
        self.scan_page(cx);
        cx.notify();
    }

    /// Fetches one page of the flat keyspace, keeps what matches, and queues the
    /// next page.
    ///
    /// A page at a time rather than one call that returns when finished: a
    /// bucket with a million keys is a thousand requests, and a scan that only
    /// shows anything at the end is one nobody can judge the cost of, or stop.
    fn scan_page(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(search)) = (
            self.client.clone(),
            self.bucket.clone(),
            self.search.as_ref(),
        ) else {
            return;
        };
        if !search.running {
            return;
        }

        let generation = self.generation;
        let token = search.continuation.clone();
        let needle = fold(&search.query);

        let listing = Tokio::spawn(cx, async move {
            client.list_flat_page(&bucket, "", token).await
        });

        self.search_task = Some(cx.spawn(async move |this, cx| {
            let outcome = listing.await;
            _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.search.is_none() {
                    return;
                }

                let failure = match outcome {
                    Ok(Ok(page)) => {
                        let matched: Vec<Entry> = page
                            .entries
                            .iter()
                            .filter(|entry| fold(&entry.name).contains(&needle))
                            .cloned()
                            .collect();

                        let held = this.entries.len();
                        if let Some(search) = this.search.as_mut() {
                            search.requests += 1;
                            search.scanned += page.entries.len();
                            search.continuation = page.continuation;
                            search.complete = search.continuation.is_none();

                            // Caps, because the person who pressed Enter is not
                            // the person who reads the bill. A bucket with ten
                            // million keys is ten thousand LIST requests, and
                            // nothing about pressing Enter said that.
                            if search.requests >= SEARCH_MAX_REQUESTS
                                || search.scanned >= SEARCH_MAX_KEYS
                                || held + matched.len() >= SEARCH_MAX_MATCHES
                            {
                                search.capped = true;
                            }
                            if search.complete || search.capped {
                                search.running = false;
                            }
                        }
                        this.entries.extend(matched);
                        this.resort_and_filter();
                        None
                    }
                    Ok(Err(error)) => Some(format!("{error:?}")),
                    Err(error) => Some(format!("Task lỗi: {error:?}")),
                };

                if let Some(message) = failure {
                    if let Some(search) = this.search.as_mut() {
                        search.running = false;
                    }
                    this.report(message);
                }

                this.status = this.search_summary();
                // Only after the state above is settled, so a stopped or
                // finished scan does not queue one more page.
                this.scan_page(cx);
                cx.notify();
            });
        }));
    }

    /// Leaves the scan where it is. The results already found stay on screen —
    /// they are real answers, and throwing them away would mean paying for them
    /// twice.
    fn stop_search(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = self.search.as_mut() {
            search.running = false;
        }
        self.search_task = None;
        self.status = self.search_summary();
        cx.notify();
    }

    /// Drops the results and goes back to browsing the prefix.
    fn exit_search(&mut self, cx: &mut Context<Self>) {
        self.search = None;
        self.search_task = None;
        match (self.bucket.clone(), self.prefix.clone()) {
            (Some(bucket), prefix) => self.open(bucket, prefix, cx),
            (None, _) => cx.notify(),
        }
    }

    /// What the scan has cost and found, worded so an unfinished scan never
    /// reads as a final answer.
    fn search_summary(&self) -> SharedString {
        let Some(search) = self.search.as_ref() else {
            return self.listing_summary();
        };
        search_summary(SearchProgress {
            found: self.entries.len(),
            scanned: search.scanned,
            requests: search.requests,
            complete: search.complete,
            capped: search.capped,
            running: search.running,
        })
        .into()
    }

    // ------------------------------------------------------ keyboard cursor

    /// The cursor's row number, if the key it names is still on screen. `None`
    /// after a filter hides it, which is why every caller has to cope with it.
    fn cursor_position(&self) -> Option<usize> {
        let cursor = self.cursor.as_deref()?;
        self.row_of(cursor)
    }

    fn row_of(&self, key: &str) -> Option<usize> {
        self.visible
            .iter()
            .position(|&index| self.entries[index].key == key)
    }

    /// Moves the cursor one row and takes the selection with it.
    fn move_cursor(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() - 1;
        let next = match self.cursor_position() {
            // Nothing under the cursor yet — the first press lands on the end
            // the key points at rather than moving from a position we do not
            // have.
            None => {
                if down {
                    0
                } else {
                    last
                }
            }
            // Clamped, not wrapping: jumping from the last row back to the
            // first reads as the selection vanishing rather than as movement.
            Some(current) if down => (current + 1).min(last),
            Some(current) => current.saturating_sub(1),
        };
        self.place_cursor(next, extend, cx);
    }

    /// Puts the cursor on a row, updates the selection to match, and scrolls it
    /// into view.
    fn place_cursor(&mut self, position: usize, extend: bool, cx: &mut Context<Self>) {
        let Some(&entry_index) = self.visible.get(position) else {
            return;
        };
        let key = self.entries[entry_index].key.clone();
        let previous = self.cursor_position();

        if extend {
            // Without an anchor the first Shift press has nothing to stretch
            // from, so the range starts where the cursor already was.
            if self.anchor.is_none() {
                self.anchor = self.cursor.clone().or_else(|| Some(key.clone()));
            }
            self.select_range_to(position);
        } else {
            self.selection.clear();
            self.selection.insert(key.clone());
            self.anchor = Some(key.clone());
        }
        self.cursor = Some(key);

        self.scroll.scroll_to_item(
            position,
            scroll_edge(previous.is_none_or(|old| position >= old)),
        );
        self.sync_inspector_to_selection(cx);
        cx.notify();
    }

    /// Selects every row between the anchor and `position`, replacing what was
    /// selected rather than adding to it: a Shift-range is one contiguous run,
    /// so shrinking it back has to actually deselect.
    fn select_range_to(&mut self, position: usize) {
        let anchor = self
            .anchor
            .as_deref()
            .and_then(|key| self.row_of(key))
            .unwrap_or(position);
        let (low, high) = if anchor <= position {
            (anchor, position)
        } else {
            (position, anchor)
        };

        self.selection.clear();
        for row in low..=high {
            if let Some(&index) = self.visible.get(row) {
                self.selection.insert(self.entries[index].key.clone());
            }
        }
    }

    /// What Enter does: a folder opens, a file previews. The same two outcomes
    /// as double-clicking, on purpose — one gesture should not reach somewhere
    /// the other cannot.
    fn open_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(position) = self.cursor_position() else {
            return;
        };
        let Some(&entry_index) = self.visible.get(position) else {
            return;
        };
        if self.entries[entry_index].is_folder {
            self.enter(entry_index, cx);
        } else {
            self.quick_look(cx);
        }
    }

    /// Navigates to a typed or pasted `s3://` path.
    ///
    /// The way in when the token cannot list buckets, which is the setup R2's
    /// own documentation recommends — so this is a main road, not a shortcut.
    /// Turns the breadcrumb into an editable path.
    fn edit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.path_input.clone() else {
            return;
        };
        // Prefilled with where we are, so the common case is editing the tail
        // rather than retyping the bucket.
        let current = match self.bucket.as_ref() {
            Some(bucket) => format!("s3://{bucket}/{}", self.prefix),
            None => String::new(),
        };
        input.update(cx, |input, cx| {
            input.set_value(current, window, cx);
            input.focus(window, cx);
        });
        self.path_choice = None;
        cx.notify();
    }

    /// Where you have been, as suggestions under the path box.
    ///
    /// The same list the Gần đây page draws, narrowed to what is typed. Here it
    /// answers a different question though: not "where have I been" but "is the
    /// place I am about to type already one keystroke away".
    fn path_suggestions(&self, cx: &App) -> Vec<Place> {
        let Some(profile) = self.places_profile() else {
            return Vec::new();
        };
        let typed = self
            .path_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        // Text that is still just *where you are* is not a filter. Both ways
        // into this box put it there — ⌘L prefills it, and the box shows the
        // current location the rest of the time — so filtering by it would
        // leave only the places underneath here, which is a suggestion list
        // that hides everywhere you have been.
        //
        // Compared rather than flagged: a flag has to be cleared, and the
        // `Change` that `set_value` itself fires arrives *after* the line that
        // sets it. That ordering is exactly what made this list come up empty.
        let here_text = self
            .bucket
            .as_ref()
            .map(|bucket| format!("s3://{bucket}/{}", self.prefix));
        let needle = if Some(&typed) == here_text.as_ref() {
            String::new()
        } else {
            fold(typed.trim_start_matches("s3://"))
        };
        let here = self.here();
        self.places
            .recent_for_profile(profile)
            .filter(|place| Some(*place) != here.as_ref())
            .filter(|place| needle.is_empty() || fold(&place.label()).contains(&needle))
            .take(PATH_SUGGESTIONS)
            .cloned()
            .collect()
    }

    fn move_path_choice(&mut self, down: bool, count: usize, cx: &mut Context<Self>) {
        if count == 0 {
            return;
        }
        self.path_choice = next_choice(self.path_choice, down, count);
        cx.notify();
    }

    fn go_to_path(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(path) = parse_s3_path(text) else {
            if !text.trim().is_empty() {
                self.fail(Failure::known(
                    "Không đọc được đường dẫn",
                    format!("{text}\nDạng đúng: s3://bucket/prefix/"),
                    None,
                ));
                cx.notify();
            }
            return;
        };

        // A region in the path that is not the profile's would reach a
        // different endpoint, so the request fails somewhere far from here with
        // a message about redirects. Better to say it while the path is still
        // on screen.
        if let Some(region) = path.region {
            let profile = self
                .active_profile
                .and_then(|index| self.profiles.get(index))
                .map(|profile| profile.region.clone());
            if profile.as_deref() != Some(region.as_str()) {
                self.fail(Failure::known(
                    "Đường dẫn ghi region khác với profile",
                    format!(
                        "Đường dẫn: {region}\nProfile: {}",
                        profile.as_deref().unwrap_or("chưa có")
                    ),
                    Some(Fix::EditProfile),
                ));
                cx.notify();
                return;
            }
        }

        self.open(path.bucket.into(), path.prefix, cx);
    }

    // ------------------------------------------------------ places

    /// Where we are, as something that can be pinned or listed.
    fn here(&self) -> Option<Place> {
        let profile = self.active_profile.and_then(|ix| self.profiles.get(ix))?;
        Some(Place {
            profile: profile.id.clone(),
            bucket: self.bucket.clone()?.to_string(),
            prefix: self.prefix.clone(),
            // Stamped on the way out, so recording a visit is what sets the
            // time. `at` is not part of a place's identity, so this is free to
            // be wrong for a pin — a pin is not a visit and never shows one.
            at: s3core::now_epoch(),
        })
    }

    /// Applies a change and writes it out.
    ///
    /// One path in, so nothing can change a preference without saving it — the
    /// worst version of a settings panel is one that forgets on restart.
    fn update_settings(&mut self, change: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        change(&mut self.settings);

        self.theme = theme_for(self.settings.theme, self.appearance, self.chrome);
        set_reduce_motion(motion_choice_reduces(self.settings.motion));
        self.transfers
            .set_bandwidth_limit(self.settings.bandwidth_limit);

        if let Some(store) = self.settings_store.as_ref() {
            if let Err(error) = store.save(&self.settings) {
                self.fail(Failure::known(
                    "Không lưu được cài đặt",
                    format!("{error}"),
                    None,
                ));
            }
        }
        cx.notify();
    }

    /// Re-lists the buckets, past the cache.
    ///
    /// Only the list. Going through `connect` would rebuild the client and
    /// navigate back to the first bucket, which is a strange thing for a button
    /// labelled "refresh the sidebar" to do.
    fn refresh_buckets(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(profile)) = (
            self.client.clone(),
            self.active_profile
                .and_then(|ix| self.profiles.get(ix))
                .map(|profile| profile.id.clone()),
        ) else {
            return;
        };

        let listing = Tokio::spawn(cx, async move { client.list_buckets_detailed().await });
        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = listing.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(listed)) => {
                        this.remember_bucket_dates(&listed);
                        let buckets: Vec<SharedString> = listed
                            .into_iter()
                            .map(|bucket| SharedString::from(bucket.name))
                            .collect();
                        this.bucket_cache
                            .insert(profile, (buckets.clone(), s3core::now_epoch()));
                        this.buckets = buckets;
                        this.buckets_cached = false;
                    }
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error:?}")),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn save_places(&mut self) {
        let Some(store) = self.place_store.as_ref() else {
            return;
        };
        if let Err(error) = store.save(&self.places) {
            // Not worth a dialog: losing a favourite is an annoyance, and the
            // list is a convenience in the first place.
            self.fail(Failure::known(
                "Không lưu được danh sách nơi đã ghim",
                format!("{error}"),
                None,
            ));
        }
    }

    fn record_visit(&mut self) {
        let Some(place) = self.here() else {
            return;
        };
        self.places.visit(place);
        self.save_places();
    }

    fn toggle_favorite(&mut self, cx: &mut Context<Self>) {
        let Some(place) = self.here() else {
            return;
        };
        let pinned = self.places.toggle_favorite(place);
        self.save_places();
        self.status = if pinned {
            "Đã ghim".into()
        } else {
            "Đã bỏ ghim".into()
        };
        cx.notify();
    }

    fn open_place(&mut self, place: &Place, cx: &mut Context<Self>) {
        // Back to the listing: opening a place from the history page and being
        // left staring at the history page reads as a click that did nothing.
        self.screen = Screen::Objects;
        self.open(place.bucket.clone().into(), place.prefix.clone(), cx);
    }

    fn known_bucket_shortcuts(&self) -> Vec<SharedString> {
        let mut seen = HashSet::new();
        let mut buckets = Vec::new();
        let mut push = |bucket: String| {
            if seen.insert(bucket.clone()) {
                buckets.push(SharedString::from(bucket));
            }
        };

        for bucket in &self.buckets {
            push(bucket.to_string());
        }
        if let Some(profile) = self.places_profile() {
            for place in self.places.for_profile(profile) {
                push(place.bucket.clone());
            }
            for place in self.places.recent_for_profile(profile) {
                push(place.bucket.clone());
            }
        }

        buckets.truncate(6);
        buckets
    }

    /// Why the sidebar's rows and the pages behind them cannot be used.
    ///
    /// One predicate for both ways in. The rows went grey and the palette kept
    /// running the very same two commands, in the same words — greying a row
    /// while ⌘K still fires it is not a block, it is a rumour.
    fn sidebar_blocked(&self) -> Option<&'static str> {
        sidebar_blocked_reason(self.client.is_some() || self.connecting)
    }

    /// Shows a page, or goes back to the listing if it is already showing.
    ///
    /// A toggle, so the same row that opened it closes it. The alternative is a
    /// page with no way back except opening something, which forces a
    /// navigation on someone who only wanted a look.
    fn show_screen(&mut self, screen: Screen, cx: &mut Context<Self>) {
        // Closing is always allowed: being stuck on a page because the thing
        // that opened it went away is worse than the page itself.
        if let (Some(reason), true) = (self.sidebar_blocked(), self.screen != screen) {
            self.status = reason.into();
            cx.notify();
            return;
        }
        self.screen = if self.screen == screen {
            Screen::Objects
        } else {
            screen
        };
        cx.notify();
    }

    /// Forgets the history for the profile whose page is open.
    ///
    /// Only this profile's: the list on screen is filtered to it, and a button
    /// that clears more than what it sits above is a button that lies.
    fn clear_recent(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.places_profile().map(str::to_string) else {
            return;
        };
        self.places.recent.retain(|place| place.profile != profile);
        self.save_places();
        self.status = "Đã xoá lịch sử".into();
        cx.notify();
    }

    /// The profile whose places are worth showing. `None` means show none:
    /// a place from another profile opens a bucket these credentials cannot see.
    fn places_profile(&self) -> Option<&str> {
        self.active_profile
            .and_then(|ix| self.profiles.get(ix))
            .map(|profile| profile.id.as_str())
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

        // Folded, like the command palette: Vietnamese file names get typed
        // without diacritics as often as with them. It also keeps filtering a
        // set of search results a no-op, because the search matched with the
        // same rule.
        let needle = fold(&self.filter);
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| needle.is_empty() || fold(&entry.name).contains(&needle))
            .map(|(index, _)| index)
            .collect();
    }

    fn toggle_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        self.sort = self.sort.toggled(key);
        self.resort_and_filter();
        // A sort S3 cannot answer by itself needs the whole prefix in hand
        // before the order means anything.
        if needs_complete_listing(self.sort) && self.continuation.is_some() {
            self.start_completing(cx);
        }
        self.status = self.listing_summary();
        cx.notify();
    }

    /// Fetches the rest of the prefix, one page at a time, into a side buffer.
    fn start_completing(&mut self, cx: &mut Context<Self>) {
        if self.completing.is_some() {
            return;
        }
        self.completing = Some(Completing {
            continuation: self.continuation.clone(),
            buffer: Vec::new(),
            requests: 0,
            running: true,
            truncated: false,
        });
        self.complete_step(cx);
    }

    fn complete_step(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(completing)) = (
            self.client.clone(),
            self.bucket.clone(),
            self.completing.as_ref(),
        ) else {
            return;
        };
        // One test for every way of being done — stopped by hand, stopped by a
        // cap, or out of pages. Treating "not running" as a plain early return
        // meant the last page set the flag and nothing ever swapped the buffer
        // in: the rows never updated and the status line said "loading" for
        // ever.
        let finished =
            !completing.running || completing.truncated || completing.continuation.is_none();
        if finished {
            return self.finish_completing(cx);
        }
        let token = completing
            .continuation
            .clone()
            .expect("a missing token is `finished` above");

        let generation = self.generation;
        let prefix = self.prefix.clone();
        let listing = Tokio::spawn(cx, async move {
            client.list_page(&bucket, &prefix, Some(token)).await
        });

        self.complete_task = Some(cx.spawn(async move |this, cx| {
            let outcome = listing.await;
            _ = this.update(cx, |this, cx| {
                if this.generation != generation || this.completing.is_none() {
                    return;
                }
                let held = this.entries.len();
                let Some(completing) = this.completing.as_mut() else {
                    return;
                };

                match outcome {
                    Ok(Ok(page)) => {
                        completing.requests += 1;
                        completing.continuation = page.continuation;
                        completing.buffer.extend(page.entries);

                        // Caps, because "load everything" on a bucket with a
                        // million keys is a thousand requests nobody agreed to.
                        // Hitting one is not a failure; it is a smaller answer,
                        // and the summary has to say which.
                        if completing.requests >= COMPLETE_MAX_REQUESTS
                            || held + completing.buffer.len() >= COMPLETE_MAX_KEYS
                        {
                            completing.truncated = true;
                        }
                        if completing.continuation.is_none() {
                            completing.running = false;
                        }
                    }
                    Ok(Err(error)) => {
                        completing.running = false;
                        completing.truncated = true;
                        this.report(format!("{error:?}"));
                    }
                    Err(error) => {
                        completing.running = false;
                        completing.truncated = true;
                        this.report(format!("Task lỗi: {error:?}"));
                    }
                }

                this.status = this.listing_summary();
                this.complete_step(cx);
                cx.notify();
            });
        }));
    }

    /// Swaps the buffered pages in and sorts once.
    fn finish_completing(&mut self, cx: &mut Context<Self>) {
        let Some(completing) = self.completing.take() else {
            return;
        };
        self.complete_task = None;

        self.entries.extend(completing.buffer);
        // Only when the whole prefix arrived. Keeping the token where a cap cut
        // it short leaves the normal paging able to carry on, and leaves
        // `listing_summary` able to say the list is still partial.
        self.continuation = completing.continuation.filter(|_| completing.truncated);
        self.resort_and_filter();
        self.status = self.listing_summary();
        cx.notify();
    }

    fn stop_completing(&mut self, cx: &mut Context<Self>) {
        if let Some(completing) = self.completing.as_mut() {
            completing.running = false;
            completing.truncated = true;
        }
        self.complete_task = None;
        self.finish_completing(cx);
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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        });
        self.op_task = Some(task);
    }

    /// Creating a bucket makes any remembered list wrong, so the entry goes.
    /// Keeps the dates a listing reported, dropping nothing already known.
    ///
    /// Merged rather than replaced because a cached restore carries no dates:
    /// overwriting with `None` would blank a column that was already filled.
    fn remember_bucket_dates(&mut self, listed: &[s3core::BucketInfo]) {
        for bucket in listed {
            if let Some(created) = bucket.created {
                self.bucket_created.insert(bucket.name.clone(), created);
            }
        }
    }

    fn forget_bucket_cache(&mut self) {
        if let Some(profile) = self
            .active_profile
            .and_then(|ix| self.profiles.get(ix))
            .map(|profile| profile.id.clone())
        {
            self.bucket_cache.remove(&profile);
        }
        self.buckets_cached = false;
    }

    fn create_bucket(&mut self, name: String, cx: &mut Context<Self>) {
        self.forget_bucket_cache();
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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        });
        self.op_task = Some(task);
    }

    /// Learns what this bucket supports, and whether versioning is switched on.
    ///
    /// Cached per bucket: four probes is a real cost to pay once, and an absurd
    /// one to pay on every navigation inside the same bucket.
    fn refresh_versioning(&mut self, bucket: SharedString, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let cached = self.caps_cache.get(&bucket);
        self.capabilities = cached;

        let checking = Tokio::spawn(cx, async move {
            let versioned = client.bucket_is_versioned(&bucket).await;
            let capabilities = match cached {
                Some(capabilities) => capabilities,
                None => client.detect_capabilities(&bucket).await,
            };
            (bucket, versioned, capabilities)
        });

        self.caps_task = Some(cx.spawn(async move |this, cx| {
            let Ok((bucket, versioned, capabilities)) = checking.await else {
                return;
            };
            _ = this.update(cx, |this, cx| {
                this.bucket_versioned = versioned;
                this.capabilities = Some(capabilities);
                this.caps_cache.insert(&bucket, capabilities);
                cx.notify();
            });
        }));
    }

    /// Whether a feature is worth showing. Unknown counts as yes: probing has
    /// not finished, and hiding a working feature for a moment is worse than
    /// showing one that turns out to be unavailable.
    fn supports(&self, pick: fn(&Capabilities) -> Support) -> bool {
        self.capabilities
            .as_ref()
            .map(|caps| pick(caps).is_usable())
            .unwrap_or(true)
    }

    fn acl_support(&self) -> Option<Support> {
        self.capabilities.as_ref().map(|caps| caps.acl)
    }

    fn acl_unavailable_failure(&self) -> Option<Failure> {
        let (summary, detail) = acl_unavailable_copy(self.acl_support())?;
        Some(Failure::known(summary, detail, None))
    }

    fn mark_acl_unsupported(&mut self) {
        let Some(bucket) = self.bucket.clone() else {
            return;
        };
        let mut capabilities = self.capabilities.unwrap_or(Capabilities {
            versioning: Support::Yes,
            tagging: Support::Yes,
            lifecycle: Support::Yes,
            object_lock: Support::Yes,
            acl: Support::No,
        });
        capabilities.acl = Support::No;
        self.capabilities = Some(capabilities);
        self.caps_cache.insert(&bucket, capabilities);
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

        let beginning = Tokio::spawn(
            cx,
            async move { s3core::sso::begin(&start_url, &region).await },
        );

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = beginning.await;
            let authorization = match outcome {
                Ok(Ok(authorization)) => authorization,
                Ok(Err(error)) => {
                    _ = this.update(cx, |this, cx| {
                        this.report(format!("{error:?}"));
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

            this.update(cx, |this, cx| this.poll_sso(authorization, cx))
                .ok();
        }));
    }

    fn poll_sso(
        &mut self,
        authorization: s3core::sso::DeviceAuthorization,
        cx: &mut Context<Self>,
    ) {
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
                        this.status = "Đã đăng nhập, chọn role".into();
                    }
                    Ok(Err(error)) => {
                        this.sso = None;
                        this.report(format!("{error:?}"));
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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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
                                "Đã nhận role, phiên hết hạn {}",
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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
    }

    fn open_form(&mut self, kind: FormKind, window: &mut Window, cx: &mut Context<Self>) {
        let editing_profile = match kind {
            FormKind::EditProfile(index) => self.profiles.get(index).cloned(),
            _ => None,
        };
        let is_profile = matches!(kind, FormKind::NewProfile | FormKind::EditProfile(_));
        let mut form = Form::new(kind, window, cx);

        if is_profile {
            // The machine already knows which region it works in; opening a new
            // profile at `us-east-1` for someone whose buckets are all in
            // Frankfurt is a redirect error waiting to happen.
            if let Some(region) = vault::system_default_region() {
                form.set_owned("Region", region, window, cx);
            }

            // Built here rather than in `Form::new` because the subscription
            // has to be owned by the browser's context, and `Form::new` only
            // has an `App`.
            let labels: Vec<&'static str> =
                Provider::ALL.into_iter().map(Provider::label).collect();
            let select = cx.new(|cx| SelectState::new(labels, None, window, cx));

            // `subscribe_in`, not `subscribe`: filling the endpoint and region
            // fields writes into text inputs, and that needs the window.
            form._provider_events = Some(cx.subscribe_in(
                &select,
                window,
                |this: &mut Self, _state, event: &SelectEvent<Vec<&'static str>>, window, cx| {
                    let SelectEvent::Confirm(Some(label)) = event else {
                        return;
                    };
                    if let Some(provider) = Provider::from_label(label) {
                        this.pick_provider(provider, window, cx);
                    }
                },
            ));
            form.provider_select = Some(select);
        }

        if let Some(profile) = editing_profile {
            form.set_owned("Tên", profile.name, window, cx);
            form.set_owned("Endpoint", profile.endpoint.unwrap_or_default(), window, cx);
            form.set_owned("Region", profile.region, window, cx);
            form.set_owned("Access key", profile.access_key, window, cx);
        }

        // Focus the first field. Nothing did this before, so every dialog
        // opened with no caret anywhere: typing went to the root handler, which
        // swallows everything except Escape and Enter, and Enter then submitted
        // a form that looked filled in and was not. Survivable with a mouse,
        // because clicking a field fixes it — and invisible for exactly that
        // reason.
        if let Some(field) = form.fields.first() {
            field.state.update(cx, |state, cx| state.focus(window, cx));
        }

        self.form = Some(form);
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
        // `EditHeaders` is exempt: clearing a header *is* the edit, not a
        // half-filled form. So is the profile dialog, which validates per field.
        let optional = matches!(
            kind,
            FormKind::NewProfile | FormKind::EditProfile(_) | FormKind::EditHeaders(_)
        );
        if first.is_empty() && !optional {
            if let Some(form) = self.form.as_mut() {
                form.error = Some("Chưa nhập gì".into());
            }
            cx.notify();
            return;
        }

        match kind {
            FormKind::NewProfile | FormKind::EditProfile(_) => return self.submit_profile_form(cx),
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
            FormKind::Duplicate(key) => {
                self.form = None;
                self.duplicate_entry(key, first, cx);
            }
            FormKind::OpenBucket => {
                let Some(path) = parse_s3_path(&first) else {
                    if let Some(form) = self.form.as_mut() {
                        form.error = Some("Nhập bucket hoặc s3://bucket/prefix/".into());
                    }
                    cx.notify();
                    return;
                };
                if let Some(region) = path.region.as_ref() {
                    let profile = self
                        .active_profile
                        .and_then(|ix| self.profiles.get(ix))
                        .map(|profile| profile.region.as_str());
                    if profile != Some(region.as_str()) {
                        if let Some(form) = self.form.as_mut() {
                            form.error = Some(
                                format!(
                                    "Path này ghi region {region}; profile đang dùng region khác"
                                )
                                .into(),
                            );
                        }
                        cx.notify();
                        return;
                    }
                }
                self.form = None;
                self.open(SharedString::from(path.bucket), path.prefix, cx);
            }
            FormKind::EditHeaders(keys) => {
                let headers = s3core::ObjectHeaders {
                    content_type: some_if_filled(first),
                    cache_control: some_if_filled(form.value("Cache-Control", cx)),
                    content_disposition: some_if_filled(form.value("Content-Disposition", cx)),
                };
                self.form = None;
                self.start_bulk("Sửa header", keys, BulkOp::Headers(headers), cx);
            }
            FormKind::AddTag => {
                let value = form.value("Giá trị", cx);
                self.form = None;
                self.add_tag(format!("{first}={value}"), cx);
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
        let kind = form.kind.clone();
        let name = form.value("Tên", cx);
        let endpoint = form.value("Endpoint", cx);
        let region = form.value("Region", cx);
        let access_key = form.value("Access key", cx);
        let secret_key = form.value("Secret key", cx);
        let editing = match kind {
            FormKind::EditProfile(index) => Some(index),
            _ => None,
        };

        // Check everything before touching the keychain, so a rejected form
        // never leaves a half-made profile behind.
        let taken: Vec<&str> = self
            .profiles
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != editing)
            .map(|(_, p)| p.name.as_str())
            .collect();
        if let Some(message) =
            validate_profile(&name, &access_key, &secret_key, &taken, editing.is_some())
        {
            if let Some(form) = self.form.as_mut() {
                form.error = Some(message.into());
            }
            cx.notify();
            return;
        }
        if secret_key.is_empty() {
            if let Some(index) = editing {
                if let Some(profile) = self.profiles.get(index) {
                    if let Err(error) = vault::secret_key(&profile.id) {
                        if let Some(form) = self.form.as_mut() {
                            form.error = Some(
                                format!("Không có secret cũ, nhập secret key mới ({error})").into(),
                            );
                        }
                        cx.notify();
                        return;
                    }
                }
            }
        }

        // A pasted endpoint often carries the bucket; keep it to open later
        // rather than letting it break the connection.
        let (endpoint, bucket_hint) = if endpoint.is_empty() {
            (String::new(), None)
        } else {
            split_endpoint(&endpoint)
        };

        let stored = StoredProfile {
            id: editing
                .and_then(|index| self.profiles.get(index))
                .map(|profile| profile.id.clone())
                .unwrap_or_else(|| vault::new_profile_id(&name, &self.profiles)),
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
        if let Some(index) = editing {
            self.update_profile(
                index,
                stored,
                (!secret_key.is_empty()).then_some(secret_key),
                cx,
            );
        } else {
            self.add_profile(stored, &secret_key, cx);
        }
        if editing.is_none() {
            if let Some(bucket) = bucket_hint {
                // The connection is still in flight; remembering it here means
                // the sidebar has something even when ListBuckets is denied.
                let bucket = SharedString::from(bucket);
                if !self.buckets.contains(&bucket) {
                    self.buckets.push(bucket);
                }
            }
        }
    }

    /// Removes a profile and the secret behind it. Asking first because the
    /// secret cannot be recovered from the app once the keychain entry is gone.
    /// Applies a provider preset to the open profile form.
    fn pick_provider(&mut self, provider: Provider, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        // A new preset invalidates whatever the last test concluded, because it
        // just changed the endpoint the test was about.
        form.probe = None;

        let form = self.form.as_ref().expect("just checked");
        form.set("Endpoint", provider.endpoint_template(), window, cx);
        form.set("Region", provider.region_template(), window, cx);
        cx.notify();
    }

    /// Connects with what is in the form, without saving anything.
    ///
    /// The alternative is what this app did before: save the profile, watch it
    /// fail, and be unable to tell a typo in the secret from a typo in the
    /// endpoint from a provider that is simply down. A test that says which is
    /// the difference between fixing it in ten seconds and giving up.
    fn test_profile_connection(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let endpoint_text = form.value("Endpoint", cx);
        let (endpoint, endpoint_bucket) = if endpoint_text.is_empty() {
            (String::new(), None)
        } else {
            split_endpoint(&endpoint_text)
        };
        let region = form.value("Region", cx);
        let probe_bucket = form.value("Bucket thử", cx);
        let access_key = form.value("Access key", cx);
        let mut secret_key = form.value("Secret key", cx);
        if secret_key.is_empty() {
            if let FormKind::EditProfile(index) = &form.kind {
                if let Some(profile) = self.profiles.get(*index) {
                    match vault::secret_key(&profile.id) {
                        Ok(secret) => secret_key = secret,
                        Err(error) => {
                            if let Some(form) = self.form.as_mut() {
                                form.error =
                                    Some(format!("Không đọc được secret cũ: {error}").into());
                            }
                            cx.notify();
                            return;
                        }
                    }
                }
            }
        }

        if access_key.is_empty() || secret_key.is_empty() {
            if let Some(form) = self.form.as_mut() {
                form.error = Some("Cần access key và secret key để thử".into());
            }
            cx.notify();
            return;
        }

        let probe_target = if probe_bucket.is_empty() {
            endpoint_bucket.map(|bucket| S3Path {
                bucket,
                prefix: String::new(),
                region: None,
            })
        } else {
            match parse_s3_path(&probe_bucket) {
                Some(path) => Some(path),
                None => {
                    if let Some(form) = self.form.as_mut() {
                        form.error =
                            Some("Bucket thử phải là bucket hoặc s3://bucket/prefix/".into());
                    }
                    cx.notify();
                    return;
                }
            }
        };
        if let Some(target) = probe_target.as_ref() {
            if let Some(target_region) = target.region.as_ref() {
                let profile_region = if region.is_empty() {
                    "us-east-1"
                } else {
                    region.as_str()
                };
                if target_region != profile_region {
                    if let Some(form) = self.form.as_mut() {
                        form.error = Some(
                            format!(
                                "Bucket thử ghi region {target_region}; form đang dùng region {profile_region}"
                            )
                            .into(),
                        );
                    }
                    cx.notify();
                    return;
                }
            }
        }

        // Through the same quirk logic a saved profile goes through, or the
        // test would be answering a question nobody asked.
        let stored = StoredProfile {
            id: "probe".into(),
            name: "probe".into(),
            endpoint: (!endpoint.is_empty()).then_some(endpoint),
            region: if region.is_empty() {
                "us-east-1".into()
            } else {
                region
            },
            path_style: false,
            relaxed_checksums: false,
            access_key: access_key.clone(),
        }
        .with_provider_defaults();

        let profile = Profile {
            name: stored.name.clone(),
            endpoint: stored.endpoint.clone(),
            region: stored.region.clone(),
            path_style: stored.path_style,
            access_key: stored.access_key.clone(),
            secret_key,
            session_token: None,
            relaxed_checksums: stored.relaxed_checksums,
        };

        if let Some(form) = self.form.as_mut() {
            form.error = None;
            form.probe = Some(Probe::Running);
        }
        cx.notify();

        let probing = Tokio::spawn(cx, async move {
            let client = S3Client::connect(&profile).await?;
            if let Some(target) = probe_target {
                let page = client
                    .list_page(&target.bucket, &target.prefix, None)
                    .await?;
                anyhow::Ok(ProfileProbe::Bucket {
                    bucket: target.bucket,
                    prefix: target.prefix,
                    entries: page.entries.len(),
                })
            } else {
                match client.list_buckets().await {
                    Ok(buckets) => anyhow::Ok(ProfileProbe::Buckets(buckets.len())),
                    Err(error) => anyhow::Ok(ProfileProbe::ListBucketsDenied(format!("{error:?}"))),
                }
            }
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = probing.await;
            _ = this.update(cx, |this, cx| {
                let Some(form) = this.form.as_mut() else {
                    return;
                };
                form.probe = Some(match outcome {
                    Ok(Ok(ProfileProbe::Buckets(count))) => {
                        Probe::Ok(format!("Kết nối được, thấy {count} bucket").into())
                    }
                    Ok(Ok(ProfileProbe::Bucket {
                        bucket,
                        prefix,
                        entries,
                    })) => Probe::Ok(profile_probe_bucket_ok(&bucket, &prefix, entries).into()),
                    // Listing denied is not a failed credential. A token scoped
                    // to one bucket signs perfectly well and is what R2's own
                    // documentation recommends; reporting it as a bad key sends
                    // people to regenerate a key that was never wrong.
                    Ok(Ok(ProfileProbe::ListBucketsDenied(detail))) => {
                        let failure = Failure::new(format!("ListBuckets: {detail}"));
                        if failure.fix == Some(Fix::EditProfile) {
                            Probe::Failed(failure.summary)
                        } else {
                            Probe::Warning(
                                "Kết nối được, nhưng token không liệt kê bucket. Nhập Bucket thử để kiểm tra bucket cụ thể."
                                    .into(),
                            )
                        }
                    }
                    Ok(Err(error)) => {
                        let failure = Failure::new(format!("{error:?}"));
                        if failure.fix == Some(Fix::EditProfile) {
                            Probe::Failed(failure.summary)
                        } else {
                            Probe::Warning(
                                format!("Kết nối được, nhưng bucket thử không đọc được: {}", failure.summary)
                                    .into(),
                            )
                        }
                    }
                    Err(error) => Probe::Failed(format!("Task lỗi: {error}").into()),
                });
                cx.notify();
            });
        }));
    }

    /// Opens the header editor on the inspected object, prefilled with what it
    /// already has.
    ///
    /// Prefilled rather than blank, because a blank field means "remove this
    /// header" once submitted — an empty editor would quietly strip everything
    /// the user did not retype.
    /// Opens the header editor on everything selected.
    ///
    /// Blank fields here, not prefilled: several objects have several current
    /// values and showing one of them would be a claim about all of them. What
    /// is typed becomes exactly what they all get.
    fn edit_headers_for_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let keys = self.selected_object_keys();

        match keys.len() {
            0 => {}
            // One object has one set of current values, so it gets them.
            1 => {
                let key = keys[0].clone();
                self.open_form(FormKind::EditHeaders(keys), window, cx);
                self.prefill_headers(&key, window, cx);
            }
            _ => self.open_form(FormKind::EditHeaders(keys), window, cx),
        }
    }

    /// Fills the header fields from what the object has now, so an untouched
    /// field submits the value it is showing rather than removing it.
    fn prefill_headers(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let head = self
            .inspector
            .as_ref()
            .filter(|inspector| inspector.key == key)
            .and_then(|inspector| inspector.head.clone());

        let Some((form, head)) = self.form.as_ref().zip(head) else {
            return;
        };
        for (label, value) in [
            ("Content-Type", head.content_type),
            ("Cache-Control", head.cache_control),
            ("Content-Disposition", head.content_disposition),
        ] {
            if let Some(value) = value {
                form.set_owned(label, value, window, cx);
            }
        }
        cx.notify();
    }

    /// Opens the header editor on the inspected object, prefilled.
    fn start_edit_headers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self
            .inspector
            .as_ref()
            .map(|inspector| inspector.key.clone())
        else {
            return;
        };
        self.open_form(FormKind::EditHeaders(vec![key.clone()]), window, cx);
        self.prefill_headers(&key, window, cx);
    }

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

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Take the keyboard back first. The root's key handler *observes* keys;
        // it does not consume them, so a focused text field keeps receiving
        // everything typed into the palette. Opening it while the caret sat in
        // the editor typed the query into the file as well — and the Enter that
        // ran the command put a newline in there too.
        self.focus.focus(window);

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
            Command::Errors => {
                self.failures_open = true;
                cx.notify();
            }
            Command::EditHeaders => {
                if let Some(window) = window {
                    self.edit_headers_for_selection(window, cx);
                }
            }
            Command::NewTab => {
                if let Some(window) = window {
                    self.new_tab(window, cx);
                }
            }
            Command::CloseTab => {
                if let Some(window) = window {
                    let index = self.active_tab;
                    self.close_tab(index, window, cx);
                }
            }
            Command::GoToPath => {
                if let Some(window) = window {
                    self.edit_path(window, cx);
                }
            }
            Command::ToggleFavorite => self.toggle_favorite(cx),
            Command::Recent => self.show_screen(Screen::Recent, cx),
            Command::Buckets => self.show_screen(Screen::Buckets, cx),
            Command::Settings => {
                self.settings_open = true;
                cx.notify();
            }
            Command::About => self.open_about(cx),
            Command::CopyPath => self.copy_location(true, cx),
            Command::CopyKey => self.copy_location(false, cx),
            Command::AclFolderPrivate => self.set_acl_recursive("private", cx),
            Command::AclFolderPublic => self.set_acl_recursive("public-read", cx),
            Command::EditText => {
                if let Some(window) = window {
                    self.start_editing(window, cx);
                }
            }
            Command::SaveText => self.save_edit(cx),
            Command::Filter => {
                if let Some(window) = window {
                    self.focus_filter(window, cx);
                }
            }
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
            Command::Duplicate => {
                if let Some(window) = window {
                    self.start_duplicate(window, cx);
                }
            }
            Command::Copy => self.copy_to_clipboard_selection(false, cx),
            Command::Cut => self.copy_to_clipboard_selection(true, cx),
            Command::Paste => self.paste(cx),
            Command::SelectAll => self.select_all(cx),
            Command::Share => self.start_share(cx),
            Command::Inspect => self.open_inspector(cx),
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

        let deleting = Tokio::spawn(
            cx,
            async move { client.delete_entries(&bucket, &doomed).await },
        );

        let task = cx.spawn(async move |this, cx| {
            let outcome = deleting.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(report)) => {
                        if report.errors.is_empty() {
                            this.status = format!("Đã xoá {} key", report.deleted).into();
                        } else {
                            // The whole reason the log holds more than one:
                            // a batch delete fails per key, and the summary
                            // has to say how many while the detail keeps
                            // every one of them.
                            this.fail(Failure::known(
                                &format!(
                                    "Xoá được {} key, {} key lỗi",
                                    report.deleted,
                                    report.errors.len()
                                ),
                                report.errors.join("\n"),
                                None,
                            ));
                        }
                        this.open(reopen.0, reopen.1, cx);
                    }
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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
        // Folders come along now. They used to be filtered out here, so a
        // selection mixing files and folders quietly downloaded only the files
        // and said nothing about the rest.
        let selected: Vec<Entry> = self
            .entries
            .iter()
            .filter(|entry| self.selection.contains(&entry.key))
            .cloned()
            .collect();
        self.download_entries(selected, cx);
    }

    fn download_current_detail(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.current_detail_key() else {
            return;
        };
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.key == key)
            .cloned()
            .unwrap_or_else(|| Entry {
                name: entry_name_of(&key),
                key,
                is_folder: false,
                size: 0,
                modified_epoch: None,
                storage_class: None,
            });
        self.download_entries(vec![entry], cx);
    }

    fn download_entries(&mut self, selected: Vec<Entry>, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        if selected.is_empty() {
            return;
        }

        let Some(destination) = dirs::download_dir().or_else(dirs::home_dir) else {
            self.report("Không tìm được thư mục Downloads".into());
            return;
        };

        let engine = self.transfers.clone();
        let prefix = self.prefix.clone();
        self.drawer_open = true;
        self.status = "Đang chuẩn bị tải xuống…".into();

        let queueing = Tokio::spawn(cx, async move {
            // (key, path under the destination). Expanding folders needs a
            // listing per folder, so it happens off the UI thread.
            let mut targets: Vec<(String, String)> = Vec::new();
            for entry in &selected {
                if entry.is_folder {
                    for key in client.list_keys_recursive(&bucket, &entry.key).await? {
                        // Relative to the prefix being viewed, so the folder
                        // arrives with its own name on top rather than as a
                        // pile of loose files.
                        let relative = key.strip_prefix(&prefix).unwrap_or(&key).to_string();
                        if !relative.is_empty() && !relative.ends_with('/') {
                            targets.push((key, relative));
                        }
                    }
                } else {
                    let name = entry.key.rsplit('/').next().unwrap_or(&entry.key);
                    targets.push((entry.key.clone(), name.to_string()));
                }
            }

            let mut ids = Vec::new();
            for (key, relative) in targets {
                ids.push(
                    engine
                        .enqueue_download_to(client.clone(), &bucket, &key, &destination, &relative)
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
                        this.status = if ids.is_empty() {
                            "Không có tệp nào để tải".into()
                        } else {
                            format!("Đang tải xuống {} tệp", ids.len()).into()
                        }
                    }
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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
        let Some(key) = self.inspection_key_for_selection() else {
            if !self.selection.is_empty() {
                self.report("Thư mục không có metadata".into());
            }
            return;
        };

        self.inspect_key(key, cx);
    }

    fn sync_inspector_to_selection(&mut self, cx: &mut Context<Self>) {
        if self.inspector.is_none() {
            return;
        }
        let Some(key) = self.inspection_key_for_selection() else {
            return;
        };
        if self
            .inspector
            .as_ref()
            .is_some_and(|inspector| inspector.key == key)
        {
            return;
        }
        self.inspect_key(key, cx);
    }

    fn inspect_key(&mut self, key: String, cx: &mut Context<Self>) {
        self.inspector = Some(Inspection {
            key: key.clone(),
            head: None,
            tags: Vec::new(),
            acl: None,
            loading: true,
            versions: Vec::new(),
        });
        self.load_inspection(key, cx);
        cx.notify();
    }

    fn load_inspection(&mut self, key: String, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };

        let loaded_key = key.clone();
        let versioned = self.bucket_versioned;
        let acl_supported = self.supports(|caps| caps.acl);
        let loading = Tokio::spawn(cx, async move {
            let head = client.head_object(&bucket, &key).await;
            // Tagging is a separate request, and a provider that does not
            // implement it must not blank out the metadata that did load.
            let tags = client.object_tags(&bucket, &key).await.unwrap_or_default();
            let versions = if versioned {
                client
                    .list_versions(&bucket, &key)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            // Skipped where ACLs are off, which is the default for buckets made
            // since 2023 — asking there is a request that can only fail.
            // Kept only if it says something: a provider that stubs ACL reads
            // would otherwise fill the panel with placeholders.
            let acl = if acl_supported {
                client
                    .object_acl(&bucket, &key)
                    .await
                    .ok()
                    .filter(|acl| acl.is_meaningful())
            } else {
                None
            };
            head.map(|head| (head, tags, versions, acl))
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = loading.await;
            _ = this.update(cx, |this, cx| {
                if let Some(inspector) = this.inspector.as_mut() {
                    if inspector.key != loaded_key {
                        return;
                    }
                    inspector.loading = false;
                    match outcome {
                        Ok(Ok((head, tags, versions, acl))) => {
                            inspector.head = Some(head);
                            inspector.tags = tags;
                            inspector.versions = versions;
                            inspector.acl = acl;
                        }
                        Ok(Err(error)) => this.report(format!("{error:?}")),
                        Err(error) => this.report(format!("Task lỗi: {error}")),
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Fetches the first slice of the object and decides what it is. Runs only
    /// on demand: a preview of every selected row would be a download per click.
    /// Opens the preview overlay on whatever is selected.
    ///
    /// Reads the selection every time rather than trusting the inspector.
    /// Trusting it is what made Space show the last object's contents after
    /// moving to another one.
    fn open_preview(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        // The one selected object, or the row the cursor is on when the mouse
        // has not been near the list at all.
        let target = self
            .selected_object_keys()
            .first()
            .cloned()
            .or_else(|| self.cursor.clone())
            .filter(|key| !key.ends_with('/'));
        let Some(key) = target else {
            return;
        };

        // The size comes from the listing, which already has it. Waiting for a
        // HEAD here is what made a preview opened in the same gesture as the
        // inspector see size zero and give up.
        let size = self
            .entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.size)
            .unwrap_or(0);
        // The content type is only known when the inspector happens to have
        // this object open; otherwise the extension decides, which is what a
        // file manager does and costs no request.
        let content_type = self
            .inspector
            .as_ref()
            .filter(|inspector| inspector.key == key)
            .and_then(|inspector| inspector.head.as_ref())
            .and_then(|head| head.content_type.clone());
        if html_opens_externally(&key, content_type.as_deref()) {
            self.open_key_externally(key, cx);
            return;
        }
        let kind = preview_kind(&key, content_type.as_deref());

        if self
            .inspector
            .as_ref()
            .is_none_or(|inspector| inspector.key != key)
        {
            self.inspect_key(key.clone(), cx);
        }

        self.previewing = Some(Previewing {
            key: key.clone(),
            name: entry_name_of(&key).into(),
            content: None,
            size,
            editing: None,
            saving: false,
        });

        // Images have to arrive whole to decode, so oversized images are
        // refused rather than fetched and shown broken. Text is fine truncated.
        let refusal = match kind {
            // Not a byte fetched: a PDF, an archive. Pulling eight megabytes
            // of one to end up drawing a label is paying for nothing.
            PreviewKind::None => Some(Preview::Handoff("Không xem trước được kiểu này".into())),
            PreviewKind::Image if size > self.settings.preview_limit_bytes() as i64 => {
                Some(Preview::Handoff(
                    format!(
                        "Ảnh lớn hơn mức xem trước {} MB",
                        self.settings.preview_limit_bytes() / (1024 * 1024)
                    )
                    .into(),
                ))
            }
            _ if size <= 0 => Some(Preview::Handoff("Tệp rỗng".into())),
            _ => None,
        };
        if let Some(refusal) = refusal {
            if let Some(previewing) = self.previewing.as_mut() {
                previewing.content = Some(refusal);
            }
            cx.notify();
            return;
        }

        let wanted = (size as u64).min(self.settings.preview_limit_bytes());
        let fetch_key = key.clone();
        let fetching = Tokio::spawn(cx, async move {
            client.get_range(&bucket, &fetch_key, 0..wanted, None).await
        });

        self.preview_task = Some(cx.spawn(async move |this, cx| {
            let outcome = fetching.await;
            _ = this.update(cx, |this, cx| {
                // A different object may have been opened while this was in
                // flight, and its bytes are not these.
                let stale = this
                    .previewing
                    .as_ref()
                    .is_none_or(|previewing| previewing.key != key);
                if stale {
                    return;
                }
                match outcome {
                    Ok(Ok(bytes)) => {
                        if let Some(previewing) = this.previewing.as_mut() {
                            previewing.content = Some(build_preview(kind, &key, bytes));
                        }
                    }
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Whether what is on screen is the whole object, byte for byte.
    ///
    /// The one question editing turns on. A preview stops at the limit, and
    /// saving back what a truncated preview holds would delete the rest of the
    /// file — quietly, and with no way back on a bucket without versioning.
    fn preview_is_whole(&self) -> bool {
        self.previewing.as_ref().is_some_and(|previewing| {
            preview_holds_whole_object(previewing.size, self.settings.preview_limit_bytes())
        })
    }

    /// Backs out of an edit, leaving the preview open on what was fetched.
    ///
    /// Not the same as closing the modal: the fetched bytes are still here, so
    /// dropping the editor is free and keeps the reader where they were.
    fn cancel_editing(&mut self, cx: &mut Context<Self>) {
        if let Some(previewing) = self.previewing.as_mut() {
            previewing.editing = None;
            previewing.saving = false;
        }
        cx.notify();
    }

    /// Opens the text in an editor, seeded with exactly what was fetched.
    fn start_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.preview_is_whole() {
            return;
        }
        let Some(text) =
            self.previewing
                .as_ref()
                .and_then(|previewing| match previewing.content.as_ref() {
                    Some(Preview::Text(text)) => Some(text.to_string()),
                    _ => None,
                })
        else {
            return;
        };

        let editor = cx.new(|cx| InputState::new(window, cx).multi_line(true).rows(18));
        editor.update(cx, |editor, cx| {
            editor.set_value(text, window, cx);
            editor.focus(window, cx);
        });
        if let Some(previewing) = self.previewing.as_mut() {
            previewing.editing = Some(editor);
        }
        cx.notify();
    }

    /// Writes the edited text back over the object.
    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        let Some((key, editor)) = self
            .previewing
            .as_ref()
            .and_then(|p| p.editing.clone().map(|editor| (p.key.clone(), editor)))
        else {
            return;
        };
        // Checked again here, not only when the editor opened: the setting can
        // change while it is open, and overwriting an object with a truncated
        // copy is not a mistake anyone recovers from without versioning.
        if !self.preview_is_whole() {
            return;
        }

        let text = editor.read(cx).value().to_string();
        if let Some(previewing) = self.previewing.as_mut() {
            previewing.saving = true;
        }
        self.status = format!("Đang lưu {}…", entry_name_of(&key)).into();

        let target = key.clone();
        let writing = Tokio::spawn(cx, async move {
            // `PutObject` writes a new object over the old one, and a new object
            // has no tags. Reading them first and putting them back is two extra
            // requests on a save; silently dropping someone's cost-allocation
            // tags because they fixed a typo is not a trade.
            let tags = client
                .object_tags(&bucket, &target)
                .await
                .unwrap_or_default();
            client
                .put_object(&bucket, &target, text.into_bytes())
                .await?;
            if !tags.is_empty() {
                client.set_object_tags(&bucket, &target, &tags).await?;
            }
            anyhow::Ok(())
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = writing.await;
            _ = this.update(cx, |this, cx| {
                if let Some(previewing) = this.previewing.as_mut() {
                    previewing.saving = false;
                }
                match outcome {
                    Ok(Ok(())) => {
                        this.status = format!("Đã lưu {}", entry_name_of(&key)).into();
                        this.previewing = None;
                        // The size and date on the row are now the old ones.
                        if let (Some(bucket), prefix) = (this.bucket.clone(), this.prefix.clone()) {
                            this.open(bucket, prefix, cx);
                        }
                    }
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error:?}")),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn close_preview(&mut self, cx: &mut Context<Self>) {
        self.previewing = None;
        self.preview_task = None;
        cx.notify();
    }

    /// Downloads to a temporary file and hands it to whatever the system opens
    /// it with.
    ///
    /// Reads the preview first, then the inspector: the preview is what is in
    /// front of the user when there is one, and for a PDF or another handoff
    /// type it is the only thing offering this at all.
    fn open_externally(&mut self, cx: &mut Context<Self>) {
        let key = match (self.previewing.as_ref(), self.inspector.as_ref()) {
            (Some(previewing), _) => previewing.key.clone(),
            (None, Some(inspector)) => inspector.key.clone(),
            (None, None) => return,
        };
        self.open_key_externally(key, cx);
    }

    fn open_key_externally(&mut self, key: String, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        // From the listing, which has it, rather than from a HEAD that may not
        // have happened. Zero means "not known here", not "empty object" — the
        // fetch asks in that case rather than assuming.
        let size = self
            .entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.size)
            .or_else(|| {
                self.inspector
                    .as_ref()
                    .and_then(|i| i.head.as_ref())
                    .map(|h| h.size)
            })
            .unwrap_or(0)
            .max(0) as u64;
        let name = entry_name_of(&key);
        // A per-run subdirectory keeps two objects with the same name from
        // overwriting each other.
        let dir = std::env::temp_dir().join(format!("s3browser-{}", std::process::id()));
        let path = dir.join(&name);

        let done = std::sync::Arc::new(AtomicU64::new(0));
        let total = std::sync::Arc::new(AtomicU64::new(size));
        self.opening = Some(Opening {
            name: SharedString::from(name),
            path: path.clone(),
            done: done.clone(),
            total: total.clone(),
        });
        // Cleared, not set: the chip below carries this now, and a status line
        // saying the same thing in smaller type is one of them going stale.
        self.status = SharedString::default();

        let target = path;
        let fetching = Tokio::spawn(cx, async move {
            use std::io::Write as _;

            // The old code took the listing's size on faith and fell through to
            // `0..0` when it had none, writing an empty file that the other app
            // then opened as a corrupt document rather than as an error.
            let bytes_total = if size > 0 {
                size
            } else {
                let head = client.head_object(&bucket, &key).await?;
                let measured = head.size.max(0) as u64;
                total.store(measured, Ordering::Relaxed);
                measured
            };

            std::fs::create_dir_all(&dir)?;
            let mut file = std::fs::File::create(&target)?;
            // In chunks rather than one request, so there is something to
            // report: a whole-object GET has exactly two observable states, and
            // neither of them is "nearly there".
            let mut offset = 0u64;
            while offset < bytes_total {
                let take = OPEN_CHUNK.min(bytes_total - offset);
                let chunk = client
                    .get_range(&bucket, &key, offset..offset + take, None)
                    .await?;
                if chunk.is_empty() {
                    break;
                }
                offset += chunk.len() as u64;
                file.write_all(&chunk)?;
                done.store(offset, Ordering::Relaxed);
            }
            file.flush()?;
            drop(file);
            anyhow::Ok(target)
        });

        self.open_task = Some(cx.spawn(async move |this, cx| {
            let outcome = fetching.await;
            _ = this.update(cx, |this, cx| {
                this.opening = None;
                match outcome {
                    Ok(Ok(path)) => match opener::open(&path) {
                        Ok(()) => this.status = format!("Đã mở {}", path.display()).into(),
                        Err(error) => this.report(format!("Không mở được: {error}")),
                    },
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
        // The chip's bar is redrawn by the same loop the transfer queue uses.
        self.start_ticking(cx);
        cx.notify();
    }

    /// Stops a handoff fetch and takes its chip away.
    fn cancel_open(&mut self, cx: &mut Context<Self>) {
        let Some(opening) = self.opening.take() else {
            return;
        };
        // Dropping the task aborts the request with it. The half-written file
        // goes too: the next open of the same object would truncate it anyway,
        // but leaving a partial file named after a real object is the sort of
        // thing that gets opened by hand later and believed.
        self.open_task = None;
        _ = std::fs::remove_file(&opening.path);
        self.status = format!("Đã huỷ tải {}", opening.name).into();
        cx.notify();
    }

    /// Space opens detail first, then the preview on a second press.
    fn quick_look(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.inspection_key_for_selection() else {
            return;
        };
        let showing = self
            .previewing
            .as_ref()
            .map(|previewing| previewing.key.clone());
        if showing.as_deref() == Some(key.as_str()) {
            return self.close_preview(cx);
        }
        if self
            .inspector
            .as_ref()
            .is_some_and(|inspector| inspector.key == key)
        {
            self.open_preview(cx);
        } else {
            self.inspect_key(key, cx);
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

        let emptying = Tokio::spawn(
            cx,
            async move { client.empty_bucket(&bucket, |_, _| {}).await },
        );

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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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
                        this.status = "Đã khôi phục version, bản cũ vẫn còn trong lịch sử".into();
                        this.load_inspection(reload, cx);
                    }
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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
                        this.status = "Đã yêu cầu khôi phục, có thể mất vài giờ".into();
                        this.load_inspection(reload, cx);
                    }
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                cx.notify();
            });
        }));
    }

    /// Applies a canned ACL to every object under the cursor's folder.
    ///
    /// The prefix has to be walked first: ACLs live on objects, and a folder is
    /// a common prefix with no ACL of its own. Bounded by the same caps as a
    /// deep search, because walking a prefix is the same LIST requests under a
    /// different name.
    fn set_acl_recursive(&mut self, canned: &'static str, cx: &mut Context<Self>) {
        if let Some(failure) = self.acl_unavailable_failure() {
            self.fail(failure);
            cx.notify();
            return;
        }
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        let target = self
            .cursor_position()
            .and_then(|position| self.visible.get(position).copied())
            .and_then(|index| self.entries.get(index))
            .filter(|entry| entry.is_folder)
            .map(|entry| entry.key.clone());
        let Some(prefix) = target else {
            return;
        };

        self.status = format!("Đang liệt kê {prefix}…").into();
        let listing_bucket = bucket.clone();
        let listing_prefix = prefix.clone();
        let listing = Tokio::spawn(cx, async move {
            let mut keys = Vec::new();
            let mut token = None;
            let mut requests = 0usize;
            loop {
                let page = client
                    .list_flat_page(&listing_bucket, &listing_prefix, token)
                    .await?;
                keys.extend(page.entries.into_iter().map(|entry| entry.key));
                requests += 1;
                token = page.continuation;
                // The same ceiling a deep search has, and for the same reason:
                // nobody agreed to a thousand requests by picking a menu item.
                if token.is_none()
                    || requests >= SEARCH_MAX_REQUESTS
                    || keys.len() >= SEARCH_MAX_KEYS
                {
                    break;
                }
            }
            anyhow::Ok((keys, token.is_some()))
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = listing.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok((keys, truncated))) => {
                        if truncated {
                            // Said before anything is written, not after: a
                            // partial pass over permissions is the kind of
                            // half-done nobody should find out about later.
                            this.fail(Failure::known(
                                "Thư mục quá lớn, chỉ đổi quyền phần đã liệt kê",
                                format!("{prefix}: {} object đầu tiên", keys.len()),
                                None,
                            ));
                        }
                        this.start_bulk("Đổi quyền", keys, BulkOp::Acl(canned), cx);
                    }
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error:?}")),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Sets a canned ACL on everything selected, or on the inspected object when
    /// nothing is.
    ///
    /// One path for one object and for four hundred: the runner reports progress
    /// and collects failures either way, and a second single-object path would
    /// be a second place for the reporting to drift.
    fn set_acl(&mut self, canned: &'static str, cx: &mut Context<Self>) {
        if let Some(failure) = self.acl_unavailable_failure() {
            self.fail(failure);
            cx.notify();
            return;
        }
        let mut keys = self.selected_object_keys();
        if keys.is_empty() {
            keys.extend(
                self.inspector
                    .as_ref()
                    .map(|inspector| inspector.key.clone()),
            );
        }
        self.start_bulk("Đổi quyền", keys, BulkOp::Acl(canned), cx);
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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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
            chosen: PRESIGN_PRESETS[1].0,
            url: None,
            temporary_credentials,
        });
        self.presign(PRESIGN_PRESETS[1].0, PRESIGN_PRESETS[1].1, cx);
        cx.notify();
    }

    fn current_detail_key(&self) -> Option<String> {
        match (self.previewing.as_ref(), self.inspector.as_ref()) {
            (Some(previewing), _) => Some(previewing.key.clone()),
            (None, Some(inspector)) => Some(inspector.key.clone()),
            (None, None) => None,
        }
    }

    fn start_share_current(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.current_detail_key() else {
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
            key,
            chosen: PRESIGN_PRESETS[1].0,
            url: None,
            temporary_credentials,
        });
        self.presign(PRESIGN_PRESETS[1].0, PRESIGN_PRESETS[1].1, cx);
        cx.notify();
    }

    fn presign(&mut self, label: &'static str, expires: Duration, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(share)) = (
            self.client.clone(),
            self.bucket.clone(),
            self.share.as_ref(),
        ) else {
            return;
        };
        // Never sign for longer than the credentials can actually honour.
        let capped = expires.min(s3core::presign_limit_for(share.temporary_credentials));
        let key = share.key.clone();
        if let Some(share) = self.share.as_mut() {
            share.chosen = label;
            share.url = None;
        }

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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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
    /// Copies what the selection is called, as a path or as a bare key.
    ///
    /// Two forms because they go to different places: `s3://bucket/key` is what
    /// the AWS CLI and every other tool take, and the bare key is what goes into
    /// code that already knows the bucket.
    ///
    /// Several selected join with newlines, which is what a shell loop or a
    /// spreadsheet wants and what one long comma-run is not.
    fn copy_location(&mut self, as_path: bool, cx: &mut Context<Self>) {
        // Folders included: a prefix is a perfectly good thing to paste into
        // `aws s3 sync`, and leaving them out would silently copy less than was
        // selected.
        let keys: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| self.selection.contains(&entry.key))
            .map(|entry| entry.key.clone())
            .collect();
        let keys = if keys.is_empty() {
            self.cursor.clone().into_iter().collect()
        } else {
            keys
        };
        if keys.is_empty() {
            return;
        }

        let Some(bucket) = self.bucket.clone() else {
            return;
        };
        let text = location_text(&bucket, &keys, as_path);

        let what = if as_path { "đường dẫn" } else { "key" };
        self.copy_to_clipboard(text, what, cx);
    }

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

    /// Fetches thumbnails for the image rows currently on screen.
    ///
    /// Only what is visible, only images, and only ones small enough to be worth
    /// the bytes: a listing of a thousand photos would otherwise be a thousand
    /// GETs, which is a real bill and a slow list. Each key is attempted once —
    /// the map records failures too, or a broken object would be re-fetched on
    /// every repaint.
    fn load_thumbnails(&mut self, range: &Range<usize>, cx: &mut Context<Self>) {
        if self.thumb_task.is_some() {
            return;
        }

        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };

        let wanted: Vec<Entry> = self
            .visible
            .get(range.clone())
            .unwrap_or_default()
            .iter()
            .filter_map(|ix| self.entries.get(*ix))
            .filter(|entry| {
                !entry.is_folder
                    && !self.thumbnails.contains_key(&entry.key)
                    && entry.size > 0
                    && entry.size <= THUMBNAIL_LIMIT
                    && preview_kind(&entry.key, None) == PreviewKind::Image
            })
            .take(THUMBNAIL_BATCH)
            .cloned()
            .collect();

        if wanted.is_empty() {
            return;
        }
        // Reserve the slots now, so a second scroll over the same rows does not
        // queue the same fetches again while the first is still in flight.
        for entry in &wanted {
            self.thumbnails.insert(entry.key.clone(), None);
        }

        let started = Instant::now();
        let fetching = Tokio::spawn(cx, async move {
            let mut loaded = Vec::new();
            for entry in wanted {
                let size = entry.size.max(0) as u64;
                if let Ok(bytes) = client.get_range(&bucket, &entry.key, 0..size, None).await {
                    if let Some(format) = image_format_for_key(&entry.key) {
                        loaded.push((
                            entry.key,
                            std::sync::Arc::new(gpui::Image::from_bytes(format, bytes)),
                        ));
                    }
                }
            }
            loaded
        });

        self.thumb_task = Some(cx.spawn(async move |this, cx| {
            let loaded = fetching.await.unwrap_or_default();
            _ = this.update(cx, |this, cx| {
                this.thumb_task = None;
                this.perf.note_thumbnails(started.elapsed(), loaded.len());
                for (key, image) in loaded {
                    this.thumbnails.insert(key, Some(image));
                }
                cx.notify();
            });
        }));
    }

    /// Puts the selection aside for a later paste. `cut` makes it a move.
    fn copy_to_clipboard_selection(&mut self, cut: bool, cx: &mut Context<Self>) {
        let Some(bucket) = self.bucket.clone() else {
            return;
        };
        let entries: Vec<Entry> = self
            .entries
            .iter()
            .filter(|entry| self.selection.contains(&entry.key))
            .cloned()
            .collect();
        if entries.is_empty() {
            return;
        }

        self.status = format!(
            "{} {} mục",
            if cut { "Đã cắt" } else { "Đã chép" },
            entries.len()
        )
        .into();
        self.clipboard = Some(Clipboard {
            bucket,
            entries,
            cut,
        });
        cx.notify();
    }

    /// Pastes into the prefix on screen.
    ///
    /// Cross-bucket works because the copy is server-side either way; a cut
    /// deletes the source only after its copy is confirmed, so an interruption
    /// leaves a duplicate rather than a hole.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket), Some(clipboard)) = (
            self.client.clone(),
            self.bucket.clone(),
            self.clipboard.clone(),
        ) else {
            return;
        };
        let prefix = self.prefix.clone();

        // Pasting where the items already are would copy each onto itself.
        if clipboard.bucket == bucket
            && clipboard
                .entries
                .iter()
                .all(|entry| parent_prefix_of(&entry.key) == prefix)
        {
            self.report("Đã ở đúng thư mục này".into());
            return;
        }

        let cut = clipboard.cut;
        let source_bucket = clipboard.bucket.to_string();
        let target_bucket = bucket.to_string();
        let entries = clipboard.entries.clone();
        self.status = format!("Đang dán {} mục…", entries.len()).into();

        let pasting = Tokio::spawn(cx, async move {
            let mut moved = 0usize;
            let mut errors: Vec<String> = Vec::new();

            for entry in &entries {
                let name = entry_name_of(&entry.key);
                if entry.is_folder {
                    let destination = format!("{prefix}{name}/");
                    // A folder is N objects; moving across buckets is not
                    // supported by `move_prefix`, so only the same-bucket case
                    // can cut. Copying works either way.
                    let outcome = if cut && source_bucket == target_bucket {
                        client
                            .move_prefix(&source_bucket, &entry.key, &destination, |_, _| {})
                            .await
                    } else {
                        client
                            .copy_prefix(&source_bucket, &entry.key, &destination, |_, _| {})
                            .await
                    };
                    match outcome {
                        Ok(report) => {
                            moved += report.moved;
                            errors.extend(report.errors);
                        }
                        Err(error) => errors.push(format!("{}: {error}", entry.key)),
                    }
                } else {
                    let destination = format!("{prefix}{name}");
                    let outcome = client
                        .copy_object(&source_bucket, &entry.key, &target_bucket, &destination)
                        .await;
                    match outcome {
                        Ok(()) => {
                            // Delete only once the copy is confirmed.
                            if cut {
                                if let Err(error) =
                                    client.delete_object(&source_bucket, &entry.key).await
                                {
                                    errors.push(format!("{}: {error}", entry.key));
                                }
                            }
                            moved += 1;
                        }
                        Err(error) => errors.push(format!("{}: {error}", entry.key)),
                    }
                }
            }
            (moved, errors)
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let (moved, errors) = pasting.await.unwrap_or((0, Vec::new()));
            _ = this.update(cx, |this, cx| {
                if errors.is_empty() {
                    this.status = format!("Đã dán {moved} mục").into();
                    // A cut is spent once pasted; a copy stays for pasting again,
                    // which is what both Finder and Explorer do.
                    if cut {
                        this.clipboard = None;
                    }
                } else {
                    this.report(format!(
                        "Dán {moved} mục, {} lỗi: {}",
                        errors.len(),
                        errors.first().cloned().unwrap_or_default()
                    ));
                }
                if let (Some(bucket), prefix) = (this.bucket.clone(), this.prefix.clone()) {
                    this.open(bucket, prefix, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        // Only what is on screen: selecting behind an active filter would act on
        // rows the user cannot see.
        self.selection = self
            .visible
            .iter()
            .filter_map(|ix| self.entries.get(*ix))
            .map(|entry| entry.key.clone())
            .collect();
        cx.notify();
    }

    /// Copies one object beside itself under a new name. Server-side, so a
    /// multi-gigabyte object never travels through this machine.
    fn duplicate_entry(&mut self, key: String, new_name: String, cx: &mut Context<Self>) {
        let (Some(client), Some(bucket)) = (self.client.clone(), self.bucket.clone()) else {
            return;
        };
        let Some(target) = renamed_key(&key, &new_name) else {
            self.report("Tên không hợp lệ".into());
            return;
        };
        if target == key {
            self.report("Tên bản sao phải khác tên gốc".into());
            return;
        }

        let bucket_name = bucket.to_string();
        let source = key.clone();
        let destination = target.clone();
        self.status = format!("Đang sao chép {}…", entry_name_of(&key)).into();

        let copying = Tokio::spawn(cx, async move {
            client
                .copy_object(&bucket_name, &source, &bucket_name, &destination)
                .await
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let outcome = copying.await;
            _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(Ok(())) => this.status = format!("Đã sao chép thành {new_name}").into(),
                    Ok(Err(error)) => this.report(format!("{error:?}")),
                    Err(error) => this.report(format!("Task lỗi: {error}")),
                }
                if let (Some(bucket), prefix) = (this.bucket.clone(), this.prefix.clone()) {
                    this.open(bucket, prefix, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Opens the duplicate dialog for the one selected object. Folders are
    /// excluded: copying a prefix is N copies, which belongs in the transfer
    /// queue rather than behind a dialog that looks instant.
    fn start_duplicate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut selected = self.selection.iter();
        let (Some(key), None) = (selected.next(), selected.next()) else {
            return;
        };
        if key.ends_with('/') {
            self.report("Chưa hỗ trợ sao chép thư mục".into());
            return;
        }
        let key = key.clone();
        self.open_form(FormKind::Duplicate(key), window, cx);
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
                    Ok(Err(error)) => this.report(format!("{error:?}")),
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
                executor
                    .timer(Duration::from_millis(TRANSFER_TICK_MS))
                    .await;
                let still_working = this.update(cx, |this, cx| {
                    cx.notify();
                    // The handoff fetch draws a progress bar too, and it is not
                    // a queue job — without this the bar sat frozen at whatever
                    // it read on the frame the download started.
                    this.transfers.has_active_work() || this.opening.is_some()
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

    fn editor_focused(&self, window: &Window, cx: &App) -> bool {
        self.previewing
            .as_ref()
            .and_then(|previewing| previewing.editing.as_ref())
            .is_some_and(|editor| editor.read(cx).focus_handle(cx).is_focused(window))
    }

    fn path_focused(&self, window: &Window, cx: &App) -> bool {
        self.path_input
            .as_ref()
            .is_some_and(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Whether what is typed goes to the filter field rather than to the list.
    fn filter_focused(&self, window: &Window, cx: &App) -> bool {
        self.filter_input
            .as_ref()
            .is_some_and(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    /// Puts the caret in the filter field. What ⌘F now means: the field is
    /// always on screen, so there is nothing to open, only somewhere to go.
    fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.filter_input.clone() {
            input.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    /// Empties the filter and hands the keyboard back to the list.
    /// Empties the sidebar's bucket filter.
    ///
    /// Its own method rather than an inline `set_value("")`: the field's text
    /// and `self.bucket_filter` are two copies of one fact, and every place
    /// that clears one has to clear the other.
    fn clear_bucket_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bucket_filter.clear();
        if let Some(input) = self.bucket_filter_input.clone() {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = self.filter_input.clone() {
            // Setting the value emits `Change`, which is what actually clears
            // `self.filter` and re-runs the filter — one path in, so the field
            // and the list can never disagree about what is being filtered.
            input.update(cx, |input, cx| input.set_value("", window, cx));
        } else {
            self.filter.clear();
            self.resort_and_filter();
            self.status = self.listing_summary();
        }
        self.focus.focus(window);
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
                    self.palette_scroll
                        .scroll_to_item(self.palette_selected, scroll_edge(true));
                    cx.notify();
                    return;
                }
                "up" => {
                    self.palette_selected = self.palette_selected.saturating_sub(1);
                    self.palette_scroll
                        .scroll_to_item(self.palette_selected, scroll_edge(false));
                    cx.notify();
                    return;
                }
                "backspace" => {
                    let mut query = query;
                    query.pop();
                    self.palette = Some(query);
                    self.palette_selected = 0;
                    self.palette_scroll
                        .scroll_to_item(0, gpui::ScrollStrategy::Top);
                    cx.notify();
                    return;
                }
                _ => {
                    if let Some(text) = keystroke.key_char.as_deref() {
                        if !text.is_empty() && !text.chars().any(char::is_control) {
                            self.palette = Some(format!("{query}{text}"));
                            self.palette_selected = 0;
                            self.palette_scroll
                                .scroll_to_item(0, gpui::ScrollStrategy::Top);
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

        // The panel dialogs close on Escape like everything else that floats.
        // Each already closes from its scrim and its Xong button, but those are
        // mouse paths — and a dialog the keyboard opened (⌘K → "Cài đặt") that
        // only the mouse can close is a trap with two entrances and one exit.
        if self.settings_open || self.about_open || self.profiles_open || self.failures_open {
            match keystroke.key.as_str() {
                "escape" => {
                    self.settings_open = false;
                    self.about_open = false;
                    self.profiles_open = false;
                    self.failures_open = false;
                    cx.notify();
                }
                // The palette stays reachable — it paints over every dialog by
                // design, and it is how the keyboard got in here.
                "k" if primary => return self.open_palette(window, cx),
                _ => {}
            }
            return;
        }

        if primary {
            match keystroke.key.as_str() {
                "f" => return self.focus_filter(window, cx),
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
                "c" => return self.copy_to_clipboard_selection(false, cx),
                "x" => return self.copy_to_clipboard_selection(true, cx),
                "v" => return self.paste(cx),
                "a" => return self.select_all(cx),
                "enter" => return self.start_rename(window, cx),
                "i" => return self.toggle_inspector(cx),
                "k" => return self.open_palette(window, cx),
                "t" => return self.new_tab(window, cx),
                "l" => return self.edit_path(window, cx),
                // ⌘1 to ⌘9, like every browser. ⌘9 is the last tab rather than
                // the ninth, which is also what every browser does and what the
                // hand means by it.
                digit if digit.len() == 1 && digit.chars().all(|c| c.is_ascii_digit()) => {
                    let Some(index) = tab_shortcut(digit, self.tabs.len()) else {
                        return;
                    };
                    return self.switch_tab(index, window, cx);
                }
                "w" => {
                    let index = self.active_tab;
                    return self.close_tab(index, window, cx);
                }
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

        // The path box takes the arrows back while it is offering places —
        // otherwise they walk the file list *behind* the open suggestion list,
        // which is both invisible and not what was asked for. Above the block
        // below rather than in the `path_focused` branch further down, because
        // that branch never sees an arrow key: this one returns first.
        if !primary && self.path_suggesting && matches!(keystroke.key.as_str(), "down" | "up") {
            let count = self.path_suggestions(cx).len();
            return self.move_path_choice(keystroke.key == "down", count, cx);
        }

        // Arrows and Enter reach the list even while the caret is in the filter
        // field. A one-line input has nothing to do with up and down, and
        // narrowing the list then walking it should be one gesture rather than
        // a trip back to the mouse to change which thing has focus.
        if !primary {
            match keystroke.key.as_str() {
                "down" => return self.move_cursor(true, keystroke.modifiers.shift, cx),
                "up" => return self.move_cursor(false, keystroke.modifiers.shift, cx),
                // Only when the caret is in neither box. In the filter Enter
                // means "search the bucket", and in the path box it means "go
                // there" — opening the highlighted row as well would throw away
                // what was just asked for, in the same keystroke.
                "enter" if !self.filter_focused(window, cx) && !self.path_focused(window, cx) => {
                    return self.open_cursor(cx)
                }
                _ => {}
            }
        }

        // The text editor takes everything: Enter is a newline there, and the
        // arrow keys move a caret. Without this they reached the list
        // underneath — Enter opening whatever row the cursor was on, in the
        // middle of typing.
        if self.editor_focused(window, cx) {
            if keystroke.key == "escape" {
                self.close_preview(cx);
            }
            return;
        }

        if self.previewing.is_some() && keystroke.key == "escape" {
            return self.close_preview(cx);
        }

        // Typing in the path box: Escape hands the keyboard back to the list,
        // everything else is the field's.
        if self.path_focused(window, cx) {
            if keystroke.key == "escape" {
                self.path_suggesting = false;
                self.path_choice = None;
                self.focus.focus(window);
                cx.notify();
            }
            return;
        }

        // Everything below is for the list, so it must not steal the keys that
        // are being typed into the field.
        if self.filter_focused(window, cx) {
            if keystroke.key == "escape" {
                return self.clear_filter(window, cx);
            }
            return;
        }

        if keystroke.key == "space" && !self.selection.is_empty() {
            return self.quick_look(cx);
        }

        // Plain Backspace only: the modified form is taken above by delete, and
        // reaching it from here would fire both.
        if !primary && keystroke.key == "backspace" {
            return self.go_up(cx);
        }

        if keystroke.key == "escape" && !self.filter.is_empty() {
            self.clear_filter(window, cx);
        }
    }

    /// Makes one row the whole selection. What a per-row action button has to do
    /// first: the actions below work on the selection, and acting on whatever
    /// happened to be selected before would be a different object entirely.
    fn select_only(&mut self, position: usize, cx: &mut Context<Self>) {
        let Some(&entry_index) = self.visible.get(position) else {
            return;
        };
        let key = self.entries[entry_index].key.clone();
        self.selection.clear();
        self.selection.insert(key.clone());
        self.cursor = Some(key.clone());
        self.anchor = Some(key);
        self.sync_inspector_to_selection(cx);
        cx.notify();
    }

    /// Adds or removes one row from the selection, leaving the rest alone.
    /// What the tick box does, and what ⌘-click already did.
    fn toggle_row(&mut self, position: usize, cx: &mut Context<Self>) {
        let Some(&entry_index) = self.visible.get(position) else {
            return;
        };
        let key = self.entries[entry_index].key.clone();
        if !self.selection.remove(&key) {
            self.selection.insert(key.clone());
        }
        self.cursor = Some(key.clone());
        self.anchor = Some(key);
        self.sync_inspector_to_selection(cx);
        cx.notify();
    }

    /// Takes a row number rather than an index into `entries`, because a
    /// Shift-range is a run of rows on screen and the two numbering schemes
    /// diverge the moment a filter is on.
    fn click_row(&mut self, position: usize, modifiers: Modifiers, cx: &mut Context<Self>) {
        let Some(&entry_index) = self.visible.get(position) else {
            return;
        };
        let key = self.entries[entry_index].key.clone();

        if modifiers.shift {
            // The mouse half of Shift-arrow. Without it a range could only ever
            // be built one row at a time from the keyboard.
            if self.anchor.is_none() {
                self.anchor = self.cursor.clone().or_else(|| Some(key.clone()));
            }
            self.select_range_to(position);
            self.cursor = Some(key);
        } else if is_primary(&modifiers) {
            if !self.selection.remove(&key) {
                self.selection.insert(key.clone());
            }
            // The cursor follows the click either way, so arrow keys carry on
            // from what was just touched instead of from wherever they left off.
            self.cursor = Some(key.clone());
            self.anchor = Some(key);
        } else {
            self.selection.clear();
            self.selection.insert(key.clone());
            self.cursor = Some(key.clone());
            self.anchor = Some(key);
        }
        self.sync_inspector_to_selection(cx);
        cx.notify();
    }

    // ----------------------------------------------------------------- render

    /// The title bar: the path, and which account it is being read with.
    ///
    /// Laid out after Brows3, which puts the `s3://` box across the top rather
    /// than hiding it behind a click. Two reasons it belongs there: it is the
    /// only way in when the token cannot list buckets, and a box that is always
    /// on screen also always says where you are.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let profile = self
            .active_profile
            .and_then(|ix| self.profiles.get(ix))
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| "Chưa chọn profile".to_string());

        div()
            .h(px(TOOLBAR_HEIGHT))
            .flex()
            .items_center()
            .gap_2()
            .pl(px(platform::toolbar_leading_inset()))
            .pr_2()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .when_some(self.path_input.clone(), |this, input| {
                this.child(div().flex_1().min_w(px(0.)).child(
                    Input::new(&input).h(px(FIELD_HEIGHT)).prefix(sized_icon(
                        "path",
                        12.,
                        theme.text_faint,
                    )),
                ))
            })
            // The account, where Brows3 puts it: the same path under two
            // profiles is two different places, so it belongs beside the path.
            .child(
                div()
                    .id("profile-switcher")
                    .h(px(FIELD_HEIGHT))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(theme.hover)
                    .hover(|this| this.bg(theme.selected))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.profiles_open = true;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text)
                            .child(SharedString::from(profile)),
                    )
                    .child(sized_icon("chevron-down", 10., theme.text_faint)),
            )
    }

    /// The tab bar.
    ///
    /// Always on screen, even with one tab. A tab strip that appears only once
    /// you already have two tabs cannot tell you that tabs exist.
    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let titles = self.tab_titles();
        let active = self.active_tab;
        let closable = self.tabs.len() > 1;

        div()
            .h(px(TAB_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .gap_0p5()
            .px_1()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .children(titles.into_iter().enumerate().map(|(index, title)| {
                let selected = index == active;
                let title_id = SharedString::from(format!("tab-title-{index}"));
                div()
                    .id(SharedString::from(format!("tab-{index}")))
                    .h(px(TAB_HEIGHT - 6.))
                    .w(px(TAB_WIDTH))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .text_xs()
                    .cursor_pointer()
                    .bg(if selected {
                        theme.selected
                    } else {
                        theme.hover
                    })
                    .text_color(if selected {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .hover(|this| this.bg(theme.selected))
                    .child(sized_icon(
                        "folder",
                        12.,
                        if selected {
                            theme.accent
                        } else {
                            theme.text_faint
                        },
                    ))
                    .child(div().flex_1().min_w(px(0.)).child(ellipsis_text(
                        title_id,
                        title.to_string(),
                        24,
                        if selected {
                            theme.text
                        } else {
                            theme.text_muted
                        },
                    )))
                    // The close control only exists while there is another tab
                    // to fall back to.
                    .when(closable, |this| {
                        this.child(
                            small_icon_button(
                                SharedString::from(format!("tab-close-{index}")),
                                "close",
                                10.,
                                theme,
                            )
                            .on_click(cx.listener(
                                move |this, event, window, cx| {
                                    // Or the click reaches the tab underneath and
                                    // selects the tab it just closed.
                                    cx.stop_propagation();
                                    _ = event;
                                    this.close_tab(index, window, cx);
                                },
                            )),
                        )
                    })
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.switch_tab(index, window, cx)
                    }))
            }))
            .child(
                div()
                    .id("tab-new")
                    .size(px(TAB_HEIGHT - 6.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|this| this.bg(theme.hover))
                    .child(sized_icon("plus", 12., theme.text_muted))
                    .on_click(cx.listener(|this, _event, window, cx| this.new_tab(window, cx))),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let current_bucket = self.bucket.clone();
        // Everything above Cài đặt needs credentials behind it. Cài đặt does
        // not, and must not be blocked: it is where a profile is made, so
        // greying it out would leave the only way out of this state greyed out
        // too.
        let blocked = self.sidebar_blocked();

        div()
            .w(px(SIDEBAR_WIDTH))
            // Fixed too, for the same reason as the inspector: a bucket list
            // squeezed to nothing is not a smaller bucket list.
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .bg(theme.panel)
            .border_r_1()
            .border_color(theme.border)
            // Always here, with or without history behind it — a nav entry that
            // comes and goes is one nobody learns the position of. The page it
            // opens says so itself when there is nothing in it.
            .child(
                sidebar_item(
                    "nav-buckets".into(),
                    "bucket",
                    "Tất cả bucket".into(),
                    self.screen == Screen::Buckets,
                    blocked,
                    theme,
                )
                // Never attached rather than attached and made to return early,
                // so the row being dead is decided in one place instead of two.
                .when(blocked.is_none(), |row| {
                    row.on_click(cx.listener(|this, _event, _window, cx| {
                        this.show_screen(Screen::Buckets, cx);
                    }))
                }),
            )
            .child(
                sidebar_item(
                    "nav-recent".into(),
                    "clock",
                    "Gần đây".into(),
                    self.screen == Screen::Recent,
                    blocked,
                    theme,
                )
                .when(blocked.is_none(), |row| {
                    row.on_click(cx.listener(|this, _event, _window, cx| {
                        this.show_screen(Screen::Recent, cx);
                    }))
                }),
            )
            .children(self.render_places(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().flex_1().min_w(px(0.)).child(
                        section_header("BUCKETS", "add-bucket", theme).on_click(cx.listener(
                            |this, _event, window, cx| {
                                if this.client.is_some() {
                                    // Opening by name rather than creating:
                                    // a scoped token is denied CreateBucket
                                    // too, and reaching a bucket that exists
                                    // is the far more common need.
                                    this.open_form(FormKind::OpenBucket, window, cx);
                                }
                            },
                        )),
                    ))
                    // Only when the list is a remembered one. A refresh button
                    // beside a list that was just fetched is a button that does
                    // nothing, and the note beside it would be a lie.
                    .when(self.buckets_cached, |this| {
                        this.child(
                            small_icon_button("buckets-refresh".into(), "refresh", 11., theme)
                                .size(px(20.))
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.refresh_buckets(cx)
                                })),
                        )
                    }),
            )
            .when(self.buckets_cached, |this| {
                this.child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(theme.text_faint)
                        // With the age, not just the fact. Half an hour behind
                        // and one minute behind are different things to be
                        // looking at, and "từ bộ nhớ tạm" alone cannot tell
                        // them apart.
                        //
                        // Here rather than in the status bar, which was the
                        // first attempt: `buckets_cached` stays true for the
                        // rest of the session, so down there the note outlived
                        // its subject and sat next to an object listing fetched
                        // seconds ago, saying nothing about it. Beside the
                        // bucket list it cannot be read as being about anything
                        // else.
                        .whitespace_nowrap()
                        .child(SharedString::from(self.bucket_cache_note())),
                )
            })
            // Only past a threshold. Under a dozen buckets the whole list is
            // on screen already, and a search box over four rows is a control
            // nobody will ever use taking space from the rows they will.
            .when(self.buckets.len() >= BUCKET_FILTER_MIN, |this| {
                this.when_some(self.bucket_filter_input.clone(), |this, input| {
                    this.child(
                        div().px_1().child(
                            Input::new(&input)
                                .h(px(FIELD_HEIGHT))
                                .prefix(sized_icon("search", 12., theme.text_faint))
                                // Same story as the object filter above:
                                // `cleanable(true)` was here and drew an
                                // invisible control.
                                .when(!self.bucket_filter.is_empty(), |this| {
                                    this.suffix(
                                        small_icon_button(
                                            "bucket-filter-clear".into(),
                                            "close",
                                            10.,
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, window, cx| {
                                                this.clear_bucket_filter(window, cx)
                                            }),
                                        ),
                                    )
                                }),
                        ),
                    )
                })
            })
            .children(
                self.visible_buckets()
                    .into_iter()
                    .map(|bucket| {
                        let selected = current_bucket.as_ref() == Some(&bucket);
                        let target = bucket.clone();
                        sidebar_item(
                            SharedString::from(format!("bucket-{bucket}")),
                            "bucket",
                            bucket,
                            selected,
                            blocked,
                            theme,
                        )
                        .when(blocked.is_none(), |row| {
                            row.on_click(cx.listener(move |this, _event, _window, cx| {
                                this.open(target.clone(), String::new(), cx);
                            }))
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            // Only while there is nothing yet: a reconnect that already has
            // names on screen should not replace them with placeholders.
            .when(self.connecting && self.buckets.is_empty(), |this| {
                this.child(skeleton_sidebar(theme))
            })
            .child(div().flex_1())
            // Above Cài đặt, where a thing you open belongs. It used to be a
            // pill in the status bar, wedged between a row of keyboard hints
            // and the version number — a control living in the one strip of the
            // window that is otherwise all read-only text.
            .child(
                sidebar_item_with_badge(
                    "queue-toggle".into(),
                    "transfer",
                    "Hàng đợi".into(),
                    queue_badge(&self.transfers.stats()),
                    self.drawer_open,
                    None,
                    theme,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.drawer_open = !this.drawer_open;
                    cx.notify();
                })),
            )
            // At the foot of the sidebar, where Brows3 has it and where every
            // app of this shape has it. Giới thiệu sits under it as a pair.
            .child(
                sidebar_item(
                    "settings-open".into(),
                    "more",
                    "Cài đặt".into(),
                    false,
                    None,
                    theme,
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.settings_open = true;
                    cx.notify();
                })),
            )
            // Beside it rather than only in the palette: the person who needs
            // the version number is the one writing a bug report, and asking
            // them to find a command palette first is where that report stops.
            .child(
                // Never blocked, for the same reason as Cài đặt: someone with
                // nothing connected is exactly who needs to read off a version
                // number and find the crash folder.
                sidebar_item(
                    "about-open".into(),
                    "info",
                    "Giới thiệu".into(),
                    false,
                    None,
                    theme,
                )
                .on_click(cx.listener(|this, _event, _window, cx| this.open_about(cx))),
            )
    }

    /// The pinned section above the bucket list.
    ///
    /// Only when it has something in it. A permanently empty heading above the
    /// bucket list would cost the buckets a row of height to say nothing.
    ///
    /// History used to sit under this, five entries deep. It has its own page
    /// now: five was never enough to find anything with, and any more than five
    /// pushed the buckets — the thing the sidebar is actually for — off the
    /// bottom.
    fn render_places(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let theme = self.theme;
        let profile = self.places_profile()?;
        let favorites: Vec<Place> = self.places.for_profile(profile).cloned().collect();
        if favorites.is_empty() {
            return None;
        }

        let row = |place: &Place, id: SharedString| {
            let target = place.clone();
            // Elided in the middle: the bucket at the front and the last
            // segment at the end are the two halves that tell two places apart,
            // and the middle is what they share. Twenty-two rather than the old
            // twenty-six, because these rows are the same size as every other
            // row now and fewer of the bigger characters fit.
            let label = elide_middle(&place.label(), 22);
            // Never blocked: this section only exists while a profile is
            // active, since a pinned place belongs to one set of credentials.
            sidebar_item(id, "star", SharedString::from(label), false, None, theme).on_click(
                cx.listener(move |this, _event, _window, cx| this.open_place(&target, cx)),
            )
        };

        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(section_label("ĐÃ GHIM", theme))
                .children(
                    favorites
                        .iter()
                        .enumerate()
                        .map(|(ix, place)| row(place, SharedString::from(format!("fav-{ix}")))),
                ),
        )
    }

    /// The bucket names the sidebar shows, after the filter.
    fn visible_buckets(&self) -> Vec<SharedString> {
        let needle = fold(&self.bucket_filter);
        self.buckets
            .iter()
            .filter(|bucket| needle.is_empty() || fold(bucket).contains(&needle))
            .cloned()
            .collect()
    }

    /// The failure log.
    ///
    /// Everything that has gone wrong this session, newest first, each with the
    /// provider's own words underneath and a button where there is one thing to
    /// do. The status bar can only hold a summary; this is where the rest of it
    /// lives, and it is reachable by clicking that summary.
    fn render_failures(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.failures_open {
            return None;
        }
        let theme = self.theme;
        let mono = self.mono_font.clone();
        let failure_cards = failure_cards(&self.failures);
        let failure_count = self.failures.len();
        let distinct_count = failure_cards.len();

        Some(
            div()
                .id("failures-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.45))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.failures_open = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("failures-dialog")
                        .w(px(FAILURE_DIALOG_WIDTH))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_xl()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .text_color(theme.text)
                                        .child(SharedString::from(format!(
                                            "Lỗi ({failure_count})"
                                        )))
                                        .when(distinct_count < failure_count, |this| {
                                            this.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.text_faint)
                                                    .child(SharedString::from(format!(
                                                        "{distinct_count} loại lỗi, đã gộp các lỗi trùng"
                                                    ))),
                                            )
                                        }),
                                )
                                .when(!self.failures.is_empty(), |this| {
                                    this.child(
                                        action_button("failures-clear", "Xoá hết", theme).on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.failures.clear();
                                                this.failures_open = false;
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .id("failures-list")
                                .flex()
                                .flex_col()
                                .gap_3()
                                .max_h(px(440.))
                                .overflow_y_scroll()
                                // Newest first: the one being asked about is
                                // almost always the one that just happened.
                                .children(failure_cards.into_iter().enumerate().map(
                                    |(index, failure)| {
                                        let detail = flatten(&failure.detail);
                                        div()
                                            .p_3()
                                            .rounded_lg()
                                            .flex()
                                            .flex_col()
                                            .gap_2()
                                            .bg(theme.panel)
                                            .border_1()
                                            .border_color(theme.border)
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_start()
                                                    .gap_2()
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w(px(0.))
                                                            .text_xs()
                                                            .text_color(theme.danger)
                                                            .child(failure.summary),
                                                    )
                                                    .when(failure.count > 1, |this| {
                                                        this.child(
                                                            div()
                                                                .flex_shrink_0()
                                                                .px_1p5()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .bg(theme.hover)
                                                                .text_xs()
                                                                .text_color(theme.text_muted)
                                                                .child(SharedString::from(
                                                                    format!("x{}", failure.count),
                                                                )),
                                                        )
                                                    })
                                                    .child(
                                                        div()
                                                            .flex_shrink_0()
                                                            .text_xs()
                                                            .text_color(theme.text_faint)
                                                            .child(SharedString::from(
                                                                format_timestamp(failure.at),
                                                            )),
                                                    ),
                                            )
                                            // The provider's own words, clipped
                                            // rather than allowed to run: one
                                            // SDK chain is a dozen lines, and
                                            // letting it push the buttons below
                                            // the fold makes the log unusable
                                            // for the failure that needs it
                                            // most. The whole thing is one
                                            // click away on the clipboard.
                                            .child(
                                                div()
                                                    .max_h(px(FAILURE_DETAIL_HEIGHT))
                                                    .overflow_hidden()
                                                    .p_2()
                                                    .rounded_md()
                                                    .bg(theme.hover)
                                                    .border_1()
                                                    .border_color(theme.border)
                                                    .font_family(mono.clone())
                                                    .text_xs()
                                                    .child(wrapped_text(
                                                        &detail,
                                                        82,
                                                        theme.text_muted,
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .flex()
                                                    .gap_2()
                                                    .when_some(failure.fix, |this, fix| {
                                                        this.child(
                                                            action_button_dyn(
                                                                SharedString::from(format!(
                                                                    "failure-fix-{index}"
                                                                )),
                                                                SharedString::from(fix.label()),
                                                                theme,
                                                            )
                                                            .on_click(cx.listener(
                                                                move |this, _event, window, cx| {
                                                                    this.apply_fix(fix, window, cx)
                                                                },
                                                            )),
                                                        )
                                                    })
                                                    // The text cannot be
                                                    // selected with a mouse in
                                                    // this UI, so without this
                                                    // the "paste it into a
                                                    // ticket" story is a story.
                                                    .child({
                                                        let detail = failure.detail.clone();
                                                        action_button_dyn(
                                                            SharedString::from(format!(
                                                                "failure-copy-{index}"
                                                            )),
                                                            "Chép chi tiết".into(),
                                                            theme,
                                                        )
                                                        .on_click(cx.listener(
                                                            move |this, _event, _window, cx| {
                                                                this.copy_to_clipboard(
                                                                    detail.clone(),
                                                                    "chi tiết lỗi",
                                                                    cx,
                                                                )
                                                            },
                                                        ))
                                                    }),
                                            )
                                    },
                                ))
                                .when(self.failures.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.text_faint)
                                            .child("Chưa có lỗi nào"),
                                    )
                                }),
                        ),
                )
                .transition_opacity_and_offset(
                    "failures-scrim-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, -8.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    /// Places under the path box, while it has the caret.
    ///
    /// Drawn from the root rather than inside the title bar so it paints over
    /// the list instead of under it — the same ordering problem the command
    /// palette had. Its own width rather than the field's, because matching a
    /// `flex_1` field means measuring a layout that is not finished yet, and a
    /// suggestion panel is allowed to be its own size.
    fn render_path_suggestions(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.path_suggesting {
            return None;
        }
        let theme = self.theme;
        let places = self.path_suggestions(cx);
        if places.is_empty() {
            return None;
        }
        let place_count = places.len();

        Some(
            div()
                .absolute()
                .top(px(TOOLBAR_HEIGHT + 2.))
                .left(px(platform::toolbar_leading_inset()))
                .w(px(PATH_SUGGEST_WIDTH))
                .py_1()
                .flex()
                .flex_col()
                .rounded_lg()
                .bg(theme.modal)
                .border_1()
                .border_color(theme.border_strong)
                .children(places.into_iter().enumerate().map(|(index, place)| {
                    let target = place.clone();
                    let active = self.path_choice == Some(index);
                    div()
                        .id(SharedString::from(format!("path-suggest-{index}")))
                        .h(px(ROW_HEIGHT))
                        .px_3()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_sm()
                        .cursor_pointer()
                        .when(active, |this| this.bg(theme.selected))
                        .hover(|this| this.bg(theme.hover))
                        .child(sized_icon("clock", 12., theme.text_faint))
                        .child(
                            div()
                                .id("acl-popup-body")
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_color(theme.text)
                                .child(SharedString::from(format!("s3://{}", target.label()))),
                        )
                        // Mouse *down*, not click: the click would land after
                        // the field had already lost focus and taken this list
                        // down with it.
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _event, window, cx| {
                                cx.stop_propagation();
                                this.path_suggesting = false;
                                this.path_choice = None;
                                this.open_place(&target, cx);
                                this.focus.focus(window);
                                cx.notify();
                            }),
                        )
                        .transition_opacity_and_offset(
                            SharedString::from(format!("path-suggest-in-{index}")),
                            delayed_transition(
                                MOTION_ROW_MS,
                                row_stagger_from_end(index, place_count),
                            ),
                            0.0,
                            1.0,
                            motion_offset(0.0, -3.0),
                            motion_offset(0.0, 0.0),
                        )
                }))
                .into_any_element(),
        )
    }

    /// Every bucket, with what the listing already knew about it.
    ///
    /// The sidebar has the same names in a column one word wide; this is where
    /// the rest of `ListBuckets` goes. There is no size column: the number does
    /// not come back with the listing and working it out means walking every
    /// object in every bucket. Brows3 has that column and its backend never
    /// fills it in — a column that reads `—` forever is furniture, not data.
    fn render_buckets_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let buckets = self.visible_buckets();
        let bucket_count = buckets.len();
        let current = self.bucket.clone();

        div()
            .id("buckets-page")
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .h(px(TOOLBAR_HEIGHT))
                    .flex_shrink_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_sm()
                            .text_color(theme.text)
                            .child("Tất cả bucket"),
                    )
                    .child(action_button("buckets-refresh", "Làm mới", theme).on_click(
                        cx.listener(|this, _event, _window, cx| {
                            this.bucket_cache_bypass = true;
                            this.refresh_buckets(cx);
                        }),
                    ))
                    .child(
                        icon_button("buckets-close", "close", theme).on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.screen = Screen::Objects;
                                cx.notify();
                            },
                        )),
                    ),
            )
            // The same column header the listing uses, so the two pages read as
            // two views of one app rather than two apps.
            .child(
                div()
                    .h(px(HEADER_HEIGHT))
                    .flex_shrink_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.text_faint)
                    .child(div().flex_1().min_w(px(0.)).child("Tên"))
                    .child(div().w(px(BUCKET_CREATED_WIDTH)).child("Ngày tạo")),
            )
            .when(buckets.is_empty(), |this| {
                this.child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(sized_icon("bucket", 24., theme.text_faint))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.text_muted)
                                .child("Chưa thấy bucket nào"),
                        ),
                )
            })
            .child(
                div()
                    .id("buckets-list")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_scroll()
                    .children(buckets.into_iter().enumerate().map(|(index, name)| {
                        let target = name.clone();
                        let selected = current.as_ref() == Some(&name);
                        let created = self
                            .bucket_created
                            .get(name.as_ref())
                            .map(|at| s3core::format_timestamp(*at))
                            .unwrap_or_else(|| "—".to_string());
                        div()
                            .id(SharedString::from(format!("bucket-row-{index}")))
                            .h(px(ROW_HEIGHT))
                            .w_full()
                            .px_3()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .cursor_pointer()
                            .when(selected, |this| this.bg(theme.selected))
                            .hover(|this| this.bg(theme.hover))
                            .child(sized_icon(
                                "bucket",
                                14.,
                                if selected {
                                    theme.accent
                                } else {
                                    theme.text_faint
                                },
                            ))
                            .child(div().flex_1().min_w(px(0.)).child(ellipsis_text(
                                SharedString::from(format!("bucket-page-name-{index}")),
                                name.clone().to_string(),
                                34,
                                theme.text,
                            )))
                            .child(
                                div()
                                    .w(px(BUCKET_CREATED_WIDTH))
                                    .whitespace_nowrap()
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(created)),
                            )
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.screen = Screen::Objects;
                                this.open(target.clone(), String::new(), cx);
                            }))
                            .transition_opacity_and_offset(
                                SharedString::from(format!("bucket-row-in-{index}")),
                                delayed_transition(
                                    MOTION_ROW_MS,
                                    row_stagger_from_end(index, bucket_count),
                                ),
                                0.0,
                                1.0,
                                motion_offset(0.0, -3.0),
                                motion_offset(0.0, 0.0),
                            )
                    })),
            )
            .transition_opacity_and_offset(
                "buckets-page-in",
                smooth_transition(MOTION_PAGE_MS),
                0.0,
                1.0,
                motion_offset(0.0, 4.0),
                motion_offset(0.0, 0.0),
            )
    }

    /// The history page.
    ///
    /// Everywhere this profile has been, newest first, filling the pane the
    /// listing normally has. It used to be five lines in the sidebar, where
    /// five was too few to find anything with and six would have pushed the
    /// bucket list off the bottom.
    ///
    /// Built out of the same measurements as the listing it stands in for —
    /// `TOOLBAR_HEIGHT` header, `ROW_HEIGHT` rows, `px_3`, `text_sm` — because
    /// a page that swaps in with taller rows and bigger type reads as a
    /// different application rather than as a different page.
    fn render_recent_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        // The place you are standing in is not somewhere to go back to, and it
        // is the newest entry after every single navigation — so it would take
        // the top row of this page permanently, to say "you are here".
        let here = self.here();
        let recent: Vec<Place> = self
            .places_profile()
            .map(|profile| {
                self.places
                    .recent_for_profile(profile)
                    .filter(|place| Some(*place) != here.as_ref())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let recent_count = recent.len();

        div()
            .id("recent-page")
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .h(px(TOOLBAR_HEIGHT))
                    .flex_shrink_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    // No count beside it. The rows are right there to be
                    // counted, and a number nobody asked for reads as filler.
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_sm()
                            .text_color(theme.text)
                            .child("Gần đây"),
                    )
                    .when(!recent.is_empty(), |this| {
                        this.child(action_button("recent-clear", "Xoá hết", theme).on_click(
                            cx.listener(|this, _event, _window, cx| this.clear_recent(cx)),
                        ))
                    })
                    .child(
                        icon_button("recent-close", "close", theme).on_click(cx.listener(
                            |this, _event, _window, cx| {
                                this.screen = Screen::Objects;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .when(recent.is_empty(), |this| {
                this.child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(sized_icon("clock", 24., theme.text_faint))
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.text_muted)
                                .child("Chưa đi đâu cả"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.text_faint)
                                .child("Thư mục bạn mở sẽ hiện ở đây"),
                        ),
                )
            })
            .child(
                div()
                    .id("recent-list")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_scroll()
                    .children(recent.iter().enumerate().map(|(ix, place)| {
                        let target = place.clone();
                        let pinned = self.places.is_favorite(place);
                        div()
                            .id(SharedString::from(format!("recent-row-{ix}")))
                            .h(px(ROW_HEIGHT))
                            .w_full()
                            .px_3()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .cursor_pointer()
                            .hover(|this| this.bg(theme.hover))
                            .child(sized_icon(
                                if pinned { "star" } else { "folder" },
                                14.,
                                if pinned {
                                    theme.accent
                                } else {
                                    theme.text_faint
                                },
                            ))
                            // The whole path, not just the last segment: two
                            // `2026/` under different buckets are one word apart
                            // and the word is at the front.
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_color(theme.text)
                                    .child(SharedString::from(target.label())),
                            )
                            .when(target.at > 0, |this| {
                                this.child(
                                    div()
                                        .whitespace_nowrap()
                                        .text_color(theme.text_faint)
                                        .child(SharedString::from(s3core::format_timestamp(
                                            target.at,
                                        ))),
                                )
                            })
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.open_place(&target, cx)
                            }))
                            .transition_opacity_and_offset(
                                SharedString::from(format!("recent-row-in-{ix}")),
                                delayed_transition(
                                    MOTION_ROW_MS,
                                    row_stagger_from_end(ix, recent_count),
                                ),
                                0.0,
                                1.0,
                                motion_offset(0.0, -3.0),
                                motion_offset(0.0, 0.0),
                            )
                    })),
            )
            .transition_opacity_and_offset(
                "recent-page-in",
                smooth_transition(MOTION_PAGE_MS),
                0.0,
                1.0,
                motion_offset(0.0, 4.0),
                motion_offset(0.0, 0.0),
            )
    }

    /// The preview overlay.
    fn render_preview(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let previewing = self.previewing.as_ref()?;
        let theme = self.theme;
        let limit_mb = self.settings.preview_limit_bytes() / (1024 * 1024);
        let kind = type_badge_of(&previewing.name);
        let truncated = matches!(
            previewing.content,
            Some(Preview::Text(_)) | Some(Preview::Table(_))
        ) && !self.preview_is_whole();
        let preview_note: SharedString = if truncated {
            format!("Chỉ hiện {limit_mb} MB đầu — không sửa trực tiếp được").into()
        } else if previewing.content.is_none() {
            "Đang tải nội dung".into()
        } else if self.preview_is_whole() {
            "Toàn bộ object".into()
        } else {
            "Xem trước".into()
        };
        let size_text: SharedString = format_size(previewing.size).into();
        let editable = previewing.editing.is_none()
            && matches!(previewing.content, Some(Preview::Text(_)))
            && self.preview_is_whole();
        let modified_text = self
            .entries
            .iter()
            .find(|entry| entry.key == previewing.key)
            .and_then(|entry| entry.modified_epoch)
            .map(format_timestamp)
            .unwrap_or_else(|| "Không rõ".into());
        let content_type = self
            .inspector
            .as_ref()
            .filter(|inspector| inspector.key == previewing.key)
            .and_then(|inspector| inspector.head.as_ref())
            .and_then(|head| head.content_type.clone())
            .filter(|value| !value.is_empty())
            // The extension is a poor stand-in for a content type, but it is
            // the only thing known here before the HEAD lands, and a file with
            // no extension has nothing to stand in with.
            .unwrap_or_else(|| {
                kind.as_ref()
                    .map(SharedString::to_string)
                    .unwrap_or_else(|| "Không rõ".into())
            });
        let state_text: SharedString = if truncated {
            format!("Giới hạn {limit_mb} MB").into()
        } else if previewing.content.is_none() {
            "Đang tải".into()
        } else if self.preview_is_whole() {
            "Đủ nội dung".into()
        } else {
            "Xem nhanh".into()
        };
        let preview_inspector = self
            .inspector
            .as_ref()
            .filter(|inspector| inspector.key == previewing.key);
        let head_loading = preview_inspector.is_some_and(|inspector| inspector.loading);
        let preview_head = preview_inspector.and_then(|inspector| inspector.head.as_ref());
        let preview_acl = preview_inspector.and_then(|inspector| inspector.acl.clone());
        let preview_tags = preview_inspector
            .map(|inspector| inspector.tags.clone())
            .unwrap_or_default();
        let acl_support = self.acl_support();
        let acl_supported = acl_support.map(Support::is_usable).unwrap_or(true);
        let acl_unavailable = acl_unavailable_copy(acl_support);
        let preview_acl_target = acl_target_label(self.selected_object_keys().len().max(1));

        let preview_content = if let Some(editor) = previewing.editing.clone() {
            div()
                .font_family(self.mono_font.clone())
                .text_xs()
                .child(Input::new(&editor).h(px(PREVIEW_BODY_HEIGHT - 48.)))
                .when(previewing.saving, |this| {
                    this.child(
                        div()
                            .pt_2()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child("Đang lưu…"),
                    )
                })
                .into_any_element()
        } else {
            match previewing.content.as_ref() {
                None => div()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(theme.text_faint)
                    .child("Đang tải…")
                    .into_any_element(),
                Some(Preview::Image(image)) => div()
                    .relative()
                    .h_full()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .child(
                        gpui::img(image.clone())
                            .absolute()
                            .inset_0()
                            .size_full()
                            .object_fit(ObjectFit::Cover)
                            .blur(px(18.))
                            .opacity(0.28),
                    )
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .bg(preview_veil(theme))
                            .opacity(0.76),
                    )
                    .child(
                        gpui::img(image.clone())
                            .max_w_full()
                            .max_h(px(PREVIEW_BODY_HEIGHT - 48.))
                            .object_fit(ObjectFit::Contain)
                            .rounded_lg(),
                    )
                    .into_any_element(),
                Some(Preview::Text(text)) => div()
                    .font_family(self.mono_font.clone())
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child(text.clone())
                    .into_any_element(),
                Some(Preview::Table(table)) => {
                    render_table(table, self.mono_font.clone(), theme).into_any_element()
                }
                Some(Preview::Handoff(reason)) => div()
                    .h_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(kind_tile(kind.clone(), KIND_TILE_LARGE, theme))
                    .child(div().text_color(theme.text).child(reason.clone()))
                    .into_any_element(),
            }
        };

        let preview_detail_body = div()
            .id("preview-detail-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                preview_detail_section("THUỘC TÍNH", theme)
                    .child(preview_detail_row("Loại", content_type, theme))
                    .child(preview_detail_row("Cỡ", size_text.to_string(), theme))
                    .child(preview_detail_row("Sửa đổi", modified_text, theme))
                    .child(preview_detail_row(
                        "Trạng thái",
                        state_text.to_string(),
                        theme,
                    ))
                    .when_some(
                        preview_head.and_then(|head| head.storage_class.clone()),
                        |this, value| this.child(preview_detail_row("Lớp lưu trữ", value, theme)),
                    )
                    .when_some(
                        preview_head.and_then(|head| head.cache_control.clone()),
                        |this, value| this.child(preview_detail_row("Cache", value, theme)),
                    )
                    .when_some(
                        preview_head.and_then(|head| head.content_disposition.clone()),
                        |this, value| this.child(preview_detail_row("Trả về", value, theme)),
                    )
                    .when_some(
                        preview_head
                            .and_then(|head| head.etag.clone())
                            .map(|etag| elide_middle(&etag.replace('"', ""), 28)),
                        |this, value| this.child(preview_detail_row("ETag", value, theme)),
                    )
                    .when(head_loading, |this| {
                        this.child(
                            div()
                                .text_color(theme.text_faint)
                                .child("Đang đọc thêm metadata…"),
                        )
                    }),
            )
            .when_some(acl_unavailable, |this, (summary, detail)| {
                this.child(
                    preview_detail_section("QUYỀN TRUY CẬP", theme)
                        .child(acl_unavailable_notice(summary, detail, theme)),
                )
            })
            .when(acl_supported, |this| {
                let public = preview_acl
                    .as_ref()
                    .is_some_and(|acl| acl.grants.iter().any(|grant| grant.public));
                let section = preview_detail_section("QUYỀN TRUY CẬP", theme).child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(acl_status_chip(public, theme))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(preview_acl_target.clone())),
                        ),
                );

                let section = if let Some(acl) = preview_acl {
                    section
                        .child(preview_detail_row(
                            "Chủ sở hữu",
                            elide_middle(&acl.owner, 28),
                            theme,
                        ))
                        .child(
                            action_button("preview-acl-table", "Bảng quyền", theme).on_click(
                                cx.listener(|this, _event, _window, cx| {
                                    this.acl_popup_open = true;
                                    cx.notify();
                                }),
                            ),
                        )
                } else {
                    section.child(div().text_color(theme.text_faint).child(if head_loading {
                        "Đang đọc ACL…"
                    } else {
                        "Chưa có ACL đọc được, vẫn có thể thử đặt ACL canned."
                    }))
                };

                this.child(
                    section
                        .child(div().text_color(theme.text_faint).child("Preset nhanh"))
                        .child(div().flex().flex_wrap().gap_1().children(
                            s3core::CANNED_ACLS.iter().map(|(canned, label)| {
                                let canned = *canned;
                                action_button_dyn(
                                    SharedString::from(format!("preview-acl-{canned}")),
                                    SharedString::from(*label),
                                    theme,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| this.set_acl(canned, cx),
                                ))
                            }),
                        )),
                )
            })
            .when(self.supports(|caps| caps.tagging), |this| {
                this.child(
                    preview_detail_section("THẺ", theme)
                        .child(div().flex().items_center().justify_end().child(
                            action_button("preview-add-tag", "Thêm", theme).on_click(cx.listener(
                                |this, _event, window, cx| {
                                    this.open_form(FormKind::AddTag, window, cx)
                                },
                            )),
                        ))
                        .children(preview_tags.iter().map(|(name, value)| {
                            div()
                                .text_color(theme.text)
                                .child(SharedString::from(format!("{name} = {value}")))
                        }))
                        .when(preview_tags.is_empty(), |this| {
                            this.child(div().text_color(theme.text_faint).child("Chưa có thẻ nào"))
                        }),
                )
            });

        let preview_detail = div()
            .id("preview-detail")
            .w(px(PREVIEW_SIDE_WIDTH))
            .h(px(PREVIEW_BODY_HEIGHT - 24.))
            .flex_shrink_0()
            .rounded_lg()
            .bg(preview_veil(theme))
            .p_3()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_hidden()
            .text_xs()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(kind_tile(kind.clone(), KIND_TILE, theme))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        ellipsis_text(
                                            SharedString::from(format!(
                                                "preview-detail-name-{}",
                                                animation_key_hash(&previewing.key)
                                            )),
                                            previewing.name.clone(),
                                            32,
                                            theme.text,
                                        )
                                        .text_xs(),
                                    )
                                    .child(div().text_color(theme.text_faint).child(preview_note)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .child(
                                detail_action_button(
                                    "preview-edit-headers",
                                    "Headers",
                                    "rename",
                                    theme,
                                )
                                .on_click(cx.listener(
                                    |this, _event, window, cx| this.start_edit_headers(window, cx),
                                )),
                            )
                            .child(
                                detail_action_button("preview-share", "Chia sẻ", "link", theme)
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.start_share_current(cx)
                                    })),
                            )
                            .child(
                                detail_action_button(
                                    "preview-open-external",
                                    "Mở app",
                                    "external",
                                    theme,
                                )
                                .on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.open_externally(cx)
                                    }),
                                ),
                            )
                            .child(
                                detail_action_button(
                                    "preview-download",
                                    "Tải xuống",
                                    "download",
                                    theme,
                                )
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| this.download_current_detail(cx),
                                )),
                            ),
                    ),
            )
            .child(preview_detail_body);

        Some(
            div()
                .id("preview-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.38))
                .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _event, _window, cx| this.close_preview(cx)))
                .child(
                    div()
                        .id("preview")
                        .w(px(PREVIEW_WIDTH))
                        .h(px(PREVIEW_HEIGHT))
                        .flex()
                        .flex_col()
                        .rounded_xl()
                        .bg(preview_shell(theme))
                        .overflow_hidden()
                        .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation())
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(
                            div()
                                .h(px(PREVIEW_TITLE_HEIGHT))
                                .flex_shrink_0()
                                .px_3()
                                .flex()
                                .items_center()
                                .gap_3()
                                .bg(preview_veil(theme))
                                .rounded_tl(px(12.))
                                .rounded_tr(px(12.))
                                .child(div().flex_1().min_w(px(0.)).text_sm().child(ellipsis_text(
                                    SharedString::from(format!(
                                        "preview-title-{}",
                                        animation_key_hash(&previewing.key)
                                    )),
                                    previewing.name.clone(),
                                    64,
                                    theme.text,
                                )))
                                .child(
                                    div()
                                        .text_xs()
                                        .whitespace_nowrap()
                                        .text_color(theme.text_faint)
                                        .child(size_text),
                                )
                                .when(editable, |this| {
                                    this.child(
                                        action_button("preview-edit", "Sửa", theme).on_click(
                                            cx.listener(|this, _event, window, cx| {
                                                this.start_editing(window, cx)
                                            }),
                                        ),
                                    )
                                })
                                .when(previewing.editing.is_some(), |this| {
                                    this.child(
                                        action_button("preview-cancel", "Huỷ", theme).on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.cancel_editing(cx)
                                            }),
                                        ),
                                    )
                                    .child(
                                        action_button("preview-save", "Lưu", theme).on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.save_edit(cx)
                                            }),
                                        ),
                                    )
                                })
                                .child(icon_button("preview-close", "close", theme).on_click(
                                    cx.listener(|this, _event, _window, cx| this.close_preview(cx)),
                                )),
                        )
                        .child(
                            div()
                                .id("preview-body")
                                .h(px(PREVIEW_BODY_HEIGHT))
                                .min_h(px(0.))
                                .p_3()
                                .flex()
                                .gap_3()
                                .child(
                                    div()
                                        .id("preview-content")
                                        .h(px(PREVIEW_BODY_HEIGHT - 24.))
                                        .flex_1()
                                        .min_w(px(0.))
                                        .rounded_lg()
                                        .bg(preview_veil(theme))
                                        .overflow_scroll()
                                        .p_3()
                                        .child(preview_content),
                                )
                                .child(preview_detail),
                        ),
                )
                .transition_opacity_and_offset(
                    "preview-scrim-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, -8.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    /// The settings panel.
    fn render_settings(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.settings_open {
            return None;
        }
        let theme = self.theme;
        let settings = self.settings.clone();
        let config_dir = self
            .settings_store
            .as_ref()
            .and_then(|store| store.path().parent().map(|dir| dir.to_path_buf()));

        Some(
            div()
                .id("settings-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.45))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.settings_open = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("settings")
                        .w(px(PROFILE_DIALOG_WIDTH))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_xl()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(div().text_color(theme.text).child("Cài đặt"))
                        .child(settings_group("GIAO DIỆN", theme))
                        .child(setting_row(
                            "Chủ đề",
                            // No note: the chip says "Theo hệ thống", which is
                            // the only part that needed explaining.
                            None,
                            div()
                                .flex()
                                .gap_1()
                                .children(ThemeChoice::ALL.into_iter().map(|choice| {
                                    choice_chip(
                                        SharedString::from(format!("theme-{}", choice.label())),
                                        choice.label().into(),
                                        settings.theme == choice,
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, _window, cx| {
                                            this.update_settings(|s| s.theme = choice, cx)
                                        },
                                    ))
                                })),
                            theme,
                        ))
                        .child(setting_row(
                            "Chuyển động",
                            Some("Ảnh hưởng tới mở panel và danh sách"),
                            div()
                                .flex()
                                .gap_1()
                                .children(MotionChoice::ALL.into_iter().map(|choice| {
                                    choice_chip(
                                        SharedString::from(format!("motion-{}", choice.label())),
                                        choice.label().into(),
                                        settings.motion == choice,
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, _window, cx| {
                                            this.update_settings(|s| s.motion = choice, cx)
                                        },
                                    ))
                                })),
                            theme,
                        ))
                        .child(settings_group("TRUYỀN TẢI", theme))
                        .child(setting_row(
                            "Băng thông",
                            Some("Trần cho cả hàng đợi"),
                            div()
                                .flex()
                                .gap_1()
                                .children(BANDWIDTH_PRESETS.into_iter().map(|limit| {
                                    choice_chip(
                                        SharedString::from(format!("bw-{limit}")),
                                        SharedString::from(bandwidth_label(limit)),
                                        settings.bandwidth_limit == limit,
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, _window, cx| {
                                            this.update_settings(|s| s.bandwidth_limit = limit, cx)
                                        },
                                    ))
                                })),
                            theme,
                        ))
                        .child(setting_row(
                            "Số luồng",
                            // Said out loud rather than letting someone change
                            // it and wonder why nothing moved faster: the limit
                            // is a semaphore's permit count, and shrinking one
                            // means taking permits back from work in flight.
                            Some("Áp dụng từ lần mở app sau"),
                            div().flex().gap_1().children(
                                crate::settings::JOB_CONCURRENCY_CHOICES
                                    .into_iter()
                                    .map(|n| {
                                        choice_chip(
                                            SharedString::from(format!("jobs-{n}")),
                                            SharedString::from(n.to_string()),
                                            settings.job_concurrency == n,
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _event, _window, cx| {
                                                this.update_settings(|s| s.job_concurrency = n, cx)
                                            }),
                                        )
                                    }),
                            ),
                            theme,
                        ))
                        .child(settings_group("XEM TRƯỚC", theme))
                        .child(setting_row(
                            "Giới hạn",
                            // Naming what it does not cover, because the number
                            // reads like a cap on everything: audio, video and
                            // PDF never load here at all — they are handed to
                            // the machine — and an image over the limit is
                            // refused rather than truncated.
                            Some("Chỉ cho văn bản và ảnh"),
                            div().flex().gap_1().children(
                                crate::settings::PREVIEW_LIMITS_MB.into_iter().map(|mb| {
                                    choice_chip(
                                        SharedString::from(format!("preview-{mb}")),
                                        SharedString::from(format!("{mb} MB")),
                                        settings.preview_limit_mb == mb,
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, _window, cx| {
                                            this.update_settings(|s| s.preview_limit_mb = mb, cx)
                                        },
                                    ))
                                }),
                            ),
                            theme,
                        ))
                        .child(settings_group("DỮ LIỆU", theme))
                        // Brows3 has this and it earns its place: the bucket
                        // list is cached for half an hour, so a bucket made in
                        // the console does not appear here until it expires,
                        // and until now the only way past that was the refresh
                        // beside the sidebar heading — which is not where
                        // anyone looks for it.
                        .child(setting_row(
                            "Bộ nhớ tạm",
                            Some("Danh sách bucket, nhớ 30 phút"),
                            action_button("settings-forget-cache", "Xoá và tải lại", theme)
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.forget_bucket_cache();
                                    this.refresh_buckets(cx);
                                    this.status = "Đã xoá bộ nhớ tạm".into();
                                    cx.notify();
                                })),
                            theme,
                        ))
                        .when_some(config_dir, |this, dir| {
                            let shown = dir.display().to_string();
                            this.child(setting_row(
                                "Thư mục cấu hình",
                                Some("Profile, nơi ghim, cài đặt, hàng đợi"),
                                action_button_dyn(
                                    "open-config".into(),
                                    SharedString::from(elide_middle(&shown, 44)),
                                    theme,
                                )
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        if let Err(error) = opener::open(&dir) {
                                            this.report(format!("Không mở được thư mục: {error}"));
                                        }
                                        cx.notify();
                                    },
                                )),
                                theme,
                            ))
                        })
                        .child(div().flex().justify_end().child(
                            action_button("settings-close", "Xong", theme).on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.settings_open = false;
                                    cx.notify();
                                },
                            )),
                        )),
                )
                .transition_opacity_and_offset(
                    "settings-scrim-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, -8.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    /// Opens About, closing whatever else was open.
    ///
    /// The palette paints over every dialog, so ⌘K from inside Cài đặt could
    /// stack a second scrim on the first — and dismissing this one revealed the
    /// settings panel still sitting underneath.
    fn open_about(&mut self, cx: &mut Context<Self>) {
        self.settings_open = false;
        self.profiles_open = false;
        self.about_open = true;
        cx.notify();
    }

    /// Which build this is, and where it keeps its files.
    ///
    /// The two paths are the point. A crash report nobody can find is a crash
    /// report nobody sends, and "look in your Application Support folder" over
    /// a support thread is where most of them are lost.
    fn render_about(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.about_open {
            return None;
        }
        let theme = self.theme;
        let crash_dir = crate::crash::report_dir();

        // Both paths get the same row: the button's label *is* the path, so it
        // answers the question without being clicked — which matters on a
        // machine where nothing is registered to open a folder.
        let path_row = |id: &'static str,
                        label: &'static str,
                        note: &'static str,
                        dir: Option<PathBuf>,
                        cx: &mut Context<Self>| {
            // 44 is what fits this dialog's control column.
            let shown = SharedString::from(dir_label(dir.as_deref(), 44));
            let control = match dir {
                Some(dir) => action_button_dyn(id.into(), shown, theme)
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        let target = revealable(&dir);
                        // Opening the parent when the folder itself is not
                        // there yet is the useful answer, but doing it in
                        // silence means the window that opens is not the one
                        // whose name is on the button.
                        if target != dir {
                            this.status = SharedString::from(format!(
                                "Chưa có {}, đã mở thư mục cha",
                                dir.display()
                            ));
                        }
                        if let Err(error) = opener::open(target) {
                            this.report(format!("Không mở được thư mục: {error}"));
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
                // Not a button. One that hovers and does nothing is a promise
                // the row cannot keep.
                None => div()
                    .text_xs()
                    .text_color(theme.text_faint)
                    .child(shown)
                    .into_any_element(),
            };
            setting_row(label, Some(note), control, theme)
        };

        Some(
            div()
                .id("about-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.45))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.about_open = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("about")
                        // The settings panel's width, not the narrow one: this
                        // dialog is mostly file paths, and eliding a path in
                        // the dialog whose job is to hand it over defeats it.
                        .w(px(PROFILE_DIALOG_WIDTH))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_xl()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(div().text_color(theme.text).child("Giới thiệu"))
                        .child(setting_row(
                            "Phiên bản",
                            None,
                            div()
                                .text_xs()
                                .text_color(theme.text)
                                .child(SharedString::from(version_line())),
                            theme,
                        ))
                        // Only the crash folder. The config folder has its own
                        // row in Cài đặt, one line above this in the sidebar,
                        // and a second copy here would be two Vietnamese
                        // literals and one elision width to keep in step.
                        .child(path_row(
                            "about-crashes",
                            "Thư mục báo cáo crash",
                            "Chỉ có tệp khi app từng tắt đột ngột",
                            crash_dir,
                            cx,
                        ))
                        .child(div().flex().justify_end().child(
                            action_button("about-close", "Xong", theme).on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.about_open = false;
                                    cx.notify();
                                },
                            )),
                        )),
                )
                .transition_opacity_and_offset(
                    "about-scrim-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, -8.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    fn render_profiles_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.profiles_open {
            return None;
        }
        let theme = self.theme;

        Some(
            div()
                .id("profiles-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.45))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.profiles_open = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("profiles-dialog")
                        .w(px(DIALOG_WIDTH))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_xl()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(div().text_color(theme.text).child("Profile"))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .max_h(px(280.))
                                .overflow_hidden()
                                .children(self.profiles.iter().enumerate().map(
                                    |(index, profile)| {
                                        let selected = self.active_profile == Some(index);
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .child(
                                                sidebar_row(
                                                    SharedString::from(format!(
                                                        "profile-{}",
                                                        profile.id
                                                    )),
                                                    SharedString::from(profile.name.clone()),
                                                    selected,
                                                    theme,
                                                )
                                                .flex_1()
                                                .on_click(cx.listener(
                                                    move |this, _event, _window, cx| {
                                                        this.profiles_open = false;
                                                        this.connect(index, cx);
                                                    },
                                                )),
                                            )
                                            .child(
                                                icon_button_dyn(
                                                    SharedString::from(format!(
                                                        "edit-profile-{}",
                                                        profile.id
                                                    )),
                                                    "rename",
                                                    theme,
                                                )
                                                .on_click(cx.listener(
                                                    move |this, _event, window, cx| {
                                                        this.profiles_open = false;
                                                        this.open_form(
                                                            FormKind::EditProfile(index),
                                                            window,
                                                            cx,
                                                        )
                                                    },
                                                )),
                                            )
                                            .child(
                                                icon_button_dyn(
                                                    SharedString::from(format!(
                                                        "rm-profile-{}",
                                                        profile.id
                                                    )),
                                                    "trash",
                                                    theme,
                                                )
                                                .on_click(cx.listener(
                                                    move |this, _event, _window, cx| {
                                                        this.ask_remove_profile(index, cx)
                                                    },
                                                )),
                                            )
                                    },
                                ))
                                .when(self.profiles.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.text_faint)
                                            .child("Chưa có profile nào"),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .justify_end()
                                .child(action_button("add-profile", "Profile mới", theme).on_click(
                                    cx.listener(|this, _event, window, cx| {
                                        this.profiles_open = false;
                                        this.open_form(FormKind::NewProfile, window, cx)
                                    }),
                                ))
                                .child(action_button("profiles-close", "Đóng", theme).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.profiles_open = false;
                                        cx.notify();
                                    }),
                                )),
                        ),
                )
                .transition_opacity_and_offset(
                    "profiles-scrim-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, -8.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let bucket = self.bucket.clone();
        let (collapsed, shown) = visible_crumbs(self.breadcrumbs(), CRUMBS_SHOWN);

        div()
            .h(px(TOOLBAR_HEIGHT))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
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
            // Breadcrumb: bucket name, then one segment per prefix level. Kept
            // alongside the path box rather than replaced by it — the box is for
            // typing a place, the crumbs are for stepping back through one.
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
                            crumb(SharedString::from("crumb-root"), bucket, theme).on_click(
                                cx.listener(move |this, _event, _window, cx| {
                                    this.open(target.clone(), String::new(), cx);
                                }),
                            ),
                        )
                    })
                    // `…` for the levels that were dropped. Not dead text: it
                    // opens the path box, which is the one place the whole path
                    // is both visible and editable. Expanding back in place
                    // would only put the overflow back.
                    .when(collapsed, |this| {
                        this.child(div().text_color(theme.text_faint).text_sm().child("/"))
                            .child(crumb("crumb-more".into(), "…".into(), theme).on_click(
                                cx.listener(|this, _event, window, cx| this.edit_path(window, cx)),
                            ))
                    })
                    .children(shown.into_iter().map(|(name, prefix)| {
                        let bucket = bucket.clone();
                        div()
                            .flex()
                            .items_center()
                            .gap_0p5()
                            .child(div().text_color(theme.text_faint).text_sm().child("/"))
                            .child(
                                crumb(SharedString::from(format!("crumb-{prefix}")), name, theme)
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        if let Some(bucket) = bucket.clone() {
                                            this.open(bucket, prefix.clone(), cx);
                                        }
                                    })),
                            )
                    }))
                    .child(div().flex_1()),
            )
            // On screen always, not behind ⌘F. A filter you cannot see is a
            // filter you forget is on, and then the list is missing rows for no
            // reason you can point at. ⌘F now moves the caret here rather than
            // opening anything.
            // Beside the path it pins, not in a menu: the answer to "is this
            // one pinned" has to be visible without opening anything.
            .when(self.bucket.is_some(), |this| {
                let pinned = self
                    .here()
                    .is_some_and(|place| self.places.is_favorite(&place));
                this.child(
                    icon_button("pin", "star", theme)
                        .text_color(if pinned {
                            theme.accent
                        } else {
                            theme.text_faint
                        })
                        .on_click(
                            cx.listener(|this, _event, _window, cx| this.toggle_favorite(cx)),
                        ),
                )
            })
            .when_some(self.filter_input.clone(), |this, input| {
                this.child(
                    // Shrinks before anything else in the toolbar. A filter
                    // box narrow enough to show six characters is still a
                    // filter box; a delete button that is not on screen is not
                    // a delete button.
                    div().w(px(FILTER_WIDTH)).min_w(px(80.)).child(
                        Input::new(&input)
                            .h(px(FIELD_HEIGHT))
                            .prefix(sized_icon("search", 13., theme.text_faint))
                            // An × inside the field, so clearing it does not
                            // mean selecting the text and deleting it.
                            //
                            // Hand-built rather than the component's
                            // `cleanable(true)`, which was here and drew
                            // *nothing*: it asks for `IconName::CircleX`, and
                            // this app serves its own hand-authored icon set,
                            // which has no such name. So the control existed,
                            // took its space, and was invisible.
                            //
                            // Routed through `clear_filter` — where every other
                            // way out of a filter goes — so it also puts focus
                            // back on the list and the arrow keys work again.
                            .when(!self.filter.is_empty(), |this| {
                                this.suffix(
                                    small_icon_button("filter-clear".into(), "close", 11., theme)
                                        .on_click(cx.listener(|this, _event, window, cx| {
                                            this.clear_filter(window, cx)
                                        })),
                                )
                            }),
                    ),
                )
            })
            // Enter starts the scan, but a shortcut is not a way in. Shown only
            // with something typed, because with an empty field there is
            // nothing to go looking for.
            .when(!self.filter.is_empty() && self.bucket.is_some(), |this| {
                this.child(
                    action_button("search-run", "Tìm cả bucket", theme).on_click(cx.listener(
                        |this, _event, _window, cx| {
                            let query = this.filter.clone();
                            this.start_search(query, cx);
                        },
                    )),
                )
            })
            .when(bucket.is_some(), |this| {
                this.child(
                    div()
                        .h(px(BUTTON_HEIGHT))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .bg(theme.hover)
                        .child(
                            icon_button("layout-list", "list", theme)
                                .h(px(BUTTON_HEIGHT))
                                .w(px(BUTTON_HEIGHT))
                                .when(self.layout == LayoutMode::List, |this| {
                                    this.bg(theme.selected)
                                })
                                .tooltip(|window, cx| {
                                    Tooltip::new("Hiển thị dạng danh sách").build(window, cx)
                                })
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.layout = LayoutMode::List;
                                    cx.notify();
                                })),
                        )
                        .child(
                            icon_button("layout-grid", "grid", theme)
                                .h(px(BUTTON_HEIGHT))
                                .w(px(BUTTON_HEIGHT))
                                .when(self.layout == LayoutMode::Grid, |this| {
                                    this.bg(theme.selected)
                                })
                                .tooltip(|window, cx| {
                                    Tooltip::new("Hiển thị dạng lưới").build(window, cx)
                                })
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.layout = LayoutMode::Grid;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when(bucket.is_some(), |this| {
                this.child(
                    action_button("new-folder", "Thư mục mới", theme).on_click(cx.listener(
                        |this, _event, window, cx| this.open_form(FormKind::NewFolder, window, cx),
                    )),
                )
            })
            .when(!self.selection.is_empty(), |this| {
                let count = self.selection.len();
                let object_count = self.selected_object_keys().len();
                this.when(object_count > 0, |this| {
                    this.child(
                        icon_button("selection-details", "info", theme)
                            .tooltip(|window, cx| {
                                Tooltip::new("Chi tiết / phân quyền").build(window, cx)
                            })
                            .on_click(
                                cx.listener(|this, _event, _window, cx| this.open_inspector(cx)),
                            ),
                    )
                })
                .child(
                    danger_button("delete", SharedString::from(format!("Xoá {count}")), theme)
                        .on_click(
                            cx.listener(|this, _event, _window, cx| this.ask_delete_selection(cx)),
                        ),
                )
            })
            // Every command, reachable with a mouse. The palette holds the ones
            // with no button of their own — new bucket, empty bucket, select
            // all, sign in with SSO, the error log — and until this existed the
            // only door to them was ⌘K, which is to say no door at all for
            // anyone who does not already know it is there.
            .child(
                icon_button("commands", "more", theme).on_click(
                    cx.listener(|this, _event, window, cx| this.open_palette(window, cx)),
                ),
            )
    }

    /// What fills the object pane when there is nothing to list. A blank area
    /// with the explanation hidden in the status bar left the user staring at
    /// black — the recovery has to be where they are already looking.
    fn render_empty_state(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.bucket.is_some() || self.client.is_none() {
            return None;
        }
        let theme = self.theme;
        // Whatever went wrong is why there are no buckets, so the empty area
        // says that rather than guessing. It used to assert "token có thể chỉ
        // có quyền trên một bucket" for every failure, which is a confident
        // lie when the real cause is a wrong key or a dead endpoint.
        let failure = self
            .buckets
            .is_empty()
            .then(|| self.failures.last())
            .flatten()
            .cloned();
        let has_failure = failure.is_some();
        let bucket_scoped = failure
            .as_ref()
            .is_some_and(|failure| failure.fix == Some(Fix::OpenBucketByName));

        if bucket_scoped {
            let known = self.known_bucket_shortcuts();
            return Some(div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(520.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_xl()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(sized_icon("bucket", 18., theme.accent))
                                .child(
                                    div()
                                        .text_color(theme.text)
                                        .child("Profile này không được liệt kê bucket"),
                                ),
                        )
                        .child(div().text_xs().child(wrapped_text(
                            "Kết nối đã thành công, nhưng token không có quyền ListBuckets. Nếu token được cấp quyền trên một vài bucket cụ thể, mở trực tiếp bằng tên bucket hoặc s3://bucket/prefix/.",
                            68,
                            theme.text_muted,
                        )))
                        .when(!known.is_empty(), |this| {
                            this.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.text_faint)
                                            .child("Bucket đã biết"),
                                    )
                                    .child(
                                        div().flex().flex_wrap().gap_2().children(
                                            known.into_iter().map(|bucket| {
                                                let target = bucket.clone();
                                                action_button_dyn(
                                                    SharedString::from(format!(
                                                        "known-bucket-{bucket}"
                                                    )),
                                                    bucket,
                                                    theme,
                                                )
                                                .on_click(cx.listener(
                                                    move |this, _event, _window, cx| {
                                                        this.open(
                                                            target.clone(),
                                                            String::new(),
                                                            cx,
                                                        )
                                                    },
                                                ))
                                            }),
                                        ),
                                    ),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    action_button("scoped-open-bucket", "Mở bucket theo tên", theme)
                                        .on_click(cx.listener(|this, _event, window, cx| {
                                            this.open_form(FormKind::OpenBucket, window, cx)
                                        })),
                                )
                                .child(action_button("scoped-errors", "Xem lỗi", theme).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.failures_open = true;
                                        cx.notify();
                                    }),
                                )),
                        ),
                )
                .transition_opacity_and_offset(
                    "scoped-bucket-state-in",
                    smooth_transition(MOTION_QUICK_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, 6.0),
                    motion_offset(0.0, 0.0),
                ));
        }

        Some(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .child(div().text_color(theme.text).child(match &failure {
                    Some(failure) => failure.summary.clone(),
                    None => SharedString::from("Chọn một bucket"),
                }))
                // Not the raw error here. The provider's chain runs for eight
                // lines and buries its one useful sentence in the middle, which
                // is a wall to be scrolled past rather than something to read.
                // The summary above says what happened; the panel keeps the
                // rest for whoever needs to paste it somewhere.
                .child(div().max_w(px(440.)).text_xs().child(wrapped_text(
                    match &failure {
                        Some(_) => "Bấm Xem lỗi để đọc nguyên văn từ provider.",
                        None => "Chọn ở cột bên trái.",
                    },
                    56,
                    theme.text_muted,
                )))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        // Skipping `OpenBucketByName`: the button below already
                        // is that, and two identical buttons side by side reads
                        // as a rendering fault.
                        .when_some(
                            failure
                                .and_then(|failure| failure.fix)
                                .filter(|fix| *fix != Fix::OpenBucketByName),
                            |this, fix| {
                                this.child(
                                    action_button_dyn(
                                        "empty-fix".into(),
                                        SharedString::from(fix.label()),
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, window, cx| {
                                            this.apply_fix(fix, window, cx)
                                        },
                                    )),
                                )
                            },
                        )
                        // Always reachable, whatever the failure was: a bucket
                        // opened by name works for a scoped token, and does no
                        // harm when the cause was something else.
                        .child(
                            action_button("empty-open-bucket", "Mở bucket theo tên", theme)
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    this.open_form(FormKind::OpenBucket, window, cx)
                                })),
                        )
                        .when(has_failure, |this| {
                            this.child(action_button("empty-errors", "Xem lỗi", theme).on_click(
                                cx.listener(|this, _event, _window, cx| {
                                    this.failures_open = true;
                                    cx.notify();
                                }),
                            ))
                        }),
                )
                .transition_opacity_and_offset(
                    "bucket-empty-state-in",
                    smooth_transition(MOTION_QUICK_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, 6.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    fn render_columns(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let sort = self.sort;
        if self.layout == LayoutMode::Grid {
            let chip = |key: SortKey, label: &'static str| {
                let active = sort.key == key;
                choice_chip(
                    SharedString::from(format!("grid-sort-{label}")),
                    SharedString::from(label),
                    active,
                    theme,
                )
                .on_click(cx.listener(move |this, _event, _window, cx| this.toggle_sort(key, cx)))
            };
            return div()
                .h(px(HEADER_HEIGHT))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .border_b_1()
                .border_color(theme.border_strong)
                .text_xs()
                .text_color(theme.text_faint)
                .child("Sắp xếp")
                .child(chip(SortKey::Name, "Tên"))
                .child(chip(SortKey::Size, "Kích thước"))
                .child(chip(SortKey::Modified, "Sửa đổi"))
                .child(div().flex_1())
                .child(SharedString::from(format!("{} cột", GRID_COLUMNS)));
        }
        let all_selected = !self.visible.is_empty()
            && self
                .visible
                .iter()
                .all(|&index| self.selection.contains(&self.entries[index].key));

        let header = |key: SortKey, label: &'static str| {
            let active = sort.key == key;
            div()
                .id(SharedString::from(format!("col-{label}")))
                .flex()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .text_xs()
                .text_color(if active { theme.text } else { theme.text_faint })
                .hover(|this| this.text_color(theme.text))
                .child(label)
                // A drawn chevron rather than ▲: a text glyph among drawn icons
                // renders at a different weight and size, which is the same
                // mismatch the emoji had.
                .when(active, |this| {
                    this.child(sized_icon(
                        if sort.ascending {
                            "chevron-up"
                        } else {
                            "chevron-down"
                        },
                        12.,
                        theme.text_muted,
                    ))
                })
        };

        div()
            .h(px(HEADER_HEIGHT))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .border_b_1()
            .border_color(theme.border_strong)
            .child(
                div()
                    .w(px(ROW_NUMBER_WIDTH))
                    .text_xs()
                    .text_color(theme.text_faint)
                    .child("#"),
            )
            // Select-all, in the place the per-row boxes are. Reflects the state
            // rather than being a plain button: a tick that stays empty while
            // every row below it is ticked is a control that lies.
            .child(
                div()
                    .id("check-all")
                    .w(px(CHECK_WIDTH))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .child(check_box(all_selected, theme))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        if this
                            .visible
                            .iter()
                            .all(|&index| this.selection.contains(&this.entries[index].key))
                        {
                            this.selection.clear();
                            cx.notify();
                        } else {
                            this.select_all(cx);
                        }
                    })),
            )
            .child(div().w(px(22.)))
            .child(div().flex_1().min_w(px(NAME_MIN_WIDTH)).child(
                header(SortKey::Name, "Tên").on_click(
                    cx.listener(|this, _event, _window, cx| this.toggle_sort(SortKey::Name, cx)),
                ),
            ))
            .child(
                div()
                    .w(px(TYPE_WIDTH))
                    .text_xs()
                    .text_color(theme.text_faint)
                    .child("Loại"),
            )
            .child(
                div()
                    .w(px(84.))
                    .child(header(SortKey::Size, "Kích thước").on_click(cx.listener(
                        |this, _event, _window, cx| this.toggle_sort(SortKey::Size, cx),
                    ))),
            )
            .child(
                div()
                    .w(px(132.))
                    .child(header(SortKey::Modified, "Sửa đổi").on_click(cx.listener(
                        |this, _event, _window, cx| this.toggle_sort(SortKey::Modified, cx),
                    ))),
            )
            .child(
                div()
                    .w(px(ACTIONS_WIDTH))
                    .flex()
                    .justify_end()
                    .text_xs()
                    .text_color(theme.text_faint)
                    .child("Thao tác"),
            )
    }

    /// The body of the object pane: placeholders, a reason there is nothing, or
    /// the rows.
    ///
    /// One place rather than three `when`s at the call site, because the three
    /// are mutually exclusive and reading them as separate conditions is what
    /// lets an empty message and a list of rows end up on screen together.
    fn render_listing(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme;

        if self.loading || self.connecting {
            return skeleton_rows(theme).into_any_element();
        }
        if self.visible.is_empty() {
            return self.render_nothing_here(cx);
        }

        div()
            .flex_1()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(div().flex_1().min_h(px(0.)).child(match self.layout {
                LayoutMode::List => self.render_rows(cx).into_any_element(),
                LayoutMode::Grid => self.render_grid(cx).into_any_element(),
            }))
            .into_any_element()
    }

    /// What is in the list, counted, under the list.
    ///
    /// After Brows3, which puts it here rather than in the status bar. The
    /// split between folders and files is the part the bare total never said:
    /// "1200 mục" is a different situation depending on whether it is twelve
    /// hundred files or twelve hundred folders.
    fn render_list_footer(&self) -> impl IntoElement {
        let theme = self.theme;
        let folders = self
            .visible
            .iter()
            .filter(|&&index| self.entries[index].is_folder)
            .count();
        let files = self.visible.len() - folders;

        div()
            .h(px(HEADER_HEIGHT))
            .flex_shrink_0()
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.text_faint)
            .child(SharedString::from(format!("{} mục", self.visible.len())))
            .child(SharedString::from(format!(
                "{folders} thư mục, {files} tệp"
            )))
            .when(self.loading_more, |this| {
                this.child(status_divider(theme)).child(
                    div().child("Đang tải thêm…").transition_opacity(
                        "list-loading-more",
                        smooth_transition(MOTION_QUICK_MS),
                        0.35,
                        1.0,
                    ),
                )
            })
    }

    /// Why the list is empty. An empty prefix and a filter that matched nothing
    /// look identical on screen and are entirely different situations — one is
    /// a fact about the bucket, the other is undone by pressing Escape.
    fn render_nothing_here(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme;
        // A scan that found nothing is its own state, and the wording turns on
        // whether it finished. "Không tìm thấy" from a scan that covered a
        // tenth of the bucket is a claim the app cannot make.
        if let Some(search) = self.search.as_ref() {
            let complete = search.complete;
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .child(div().text_color(theme.text_muted).child(
                    if search.capped {
                        "Chưa thấy gì, và đã quét tới mức trần"
                    } else if complete {
                        "Không có object nào khớp"
                    } else if search.running {
                        "Đang quét…"
                    } else {
                        "Chưa thấy gì, và đã dừng giữa chừng"
                    },
                ))
                .child(div().text_xs().text_color(theme.text_faint).child(
                    SharedString::from(if complete {
                        format!(
                            "Đã quét hết bucket: {} mục trong {} yêu cầu.",
                            search.scanned, search.requests
                        )
                    } else if search.capped {
                        format!(
                            "Đã quét {} mục trong {} yêu cầu rồi dừng theo mức trần. Thu hẹp prefix rồi tìm lại.",
                            search.scanned, search.requests
                        )
                    } else {
                        format!(
                            "Mới quét {} mục trong {} yêu cầu, chưa hết bucket.",
                            search.scanned, search.requests
                        )
                    }),
                ))
                .transition_opacity_and_offset(
                    "search-empty-state-in",
                    smooth_transition(MOTION_QUICK_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, 6.0),
                    motion_offset(0.0, 0.0),
                )
                .into_any_element();
        }

        let filtered_out = emptiness(&self.filter, self.entries.len()) == Emptiness::Filtered;

        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(div().text_color(theme.text_muted).child(if filtered_out {
                "Không có mục nào khớp"
            } else {
                "Thư mục trống"
            }))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.text_faint)
                    .child(SharedString::from(if filtered_out {
                        format!(
                            "Bộ lọc “{}” không khớp mục nào trong {} mục đã tải",
                            self.filter,
                            self.entries.len()
                        )
                    } else {
                        "Kéo tệp vào đây để tải lên".to_string()
                    })),
            )
            .when(filtered_out, |this| {
                this.child(
                    action_button("empty-clear-filter", "Xoá bộ lọc", theme).on_click(
                        cx.listener(|this, _event, window, cx| this.clear_filter(window, cx)),
                    ),
                )
            })
            .transition_opacity_and_offset(
                "listing-empty-state-in",
                smooth_transition(MOTION_QUICK_MS),
                0.0,
                1.0,
                motion_offset(0.0, 6.0),
                motion_offset(0.0, 0.0),
            )
            .into_any_element()
    }

    /// The strip that says a scan is on, what it has cost, and how to leave.
    ///
    /// Above the list rather than in the status bar, because while it is up the
    /// list is not showing the folder the breadcrumb names — and that is a big
    /// enough lie to need saying where the eye already is.
    fn render_search_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let search = self.search.as_ref()?;
        let theme = self.theme;
        let running = search.running;

        Some(
            div()
                .h(px(CONTROL_BAR_HEIGHT))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .bg(theme.selected)
                .border_b_1()
                .border_color(theme.border)
                .text_xs()
                .child(sized_icon("search", 13., theme.text_muted))
                .child(
                    div()
                        .text_color(theme.text)
                        .child(SharedString::from(format!("Tìm “{}”", search.query))),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_color(theme.text_muted)
                        .child(self.search_summary()),
                )
                // Only while it is running: a stopped scan has nothing to stop,
                // and a dead button is one more thing to read past.
                .when(running, |this| {
                    this.child(
                        action_button("search-stop", "Dừng", theme).on_click(
                            cx.listener(|this, _event, _window, cx| this.stop_search(cx)),
                        ),
                    )
                })
                .child(
                    action_button("search-exit", "Thoát", theme)
                        .on_click(cx.listener(|this, _event, _window, cx| this.exit_search(cx))),
                )
                .transition_opacity_and_offset(
                    "search-strip-in",
                    smooth_transition(MOTION_QUICK_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, -4.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    fn render_rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        uniform_list(
            "objects",
            self.visible.len(),
            cx.processor(move |this: &mut Self, range: Range<usize>, _window, cx| {
                this.perf.note_visible(&range);
                this.maybe_prefetch(&range, cx);
                this.load_thumbnails(&range, cx);

                let range_end = range.end;
                range
                    .map(|position| {
                        let entry_index = this.visible[position];
                        let entry = &this.entries[entry_index];
                        let selected = this.selection.contains(&entry.key);
                        let is_cursor = this.cursor.as_deref() == Some(entry.key.as_str());
                        let is_folder = entry.is_folder;
                        let right_click_key = entry.key.clone();

                        let thumbnail = this.thumbnails.get(&entry.key).cloned().flatten();
                        // What the menu may offer depends on the row and on the
                        // clipboard, so it is decided per row rather than once.
                        let single = this.selection.len() <= 1;
                        let can_paste = this.clipboard.is_some();
                        let acl_supported = this.supports(|caps| caps.acl);
                        let actions_visible =
                            selected || is_cursor || this.hovered_row == Some(position);
                        let checkbox = div()
                            .id(("check", position))
                            .w(px(CHECK_WIDTH))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .child(check_box(selected, theme))
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                // Or the row underneath also fires and turns a
                                // tick into "select only this one".
                                cx.stop_propagation();
                                this.toggle_row(position, cx);
                            }))
                            .into_any_element();

                        // Straight to the things worth one click, rather than
                        // select-then-travel-to-the-toolbar. Only what applies:
                        // a folder has nothing to preview or share, so its row
                        // stays quieter. Hidden rows still reserve the column
                        // width; showing the buttons must not shove the name.
                        let actions = if actions_visible {
                            let actions = div()
                                .w(px(ACTIONS_WIDTH))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap_0p5();

                            let actions = if is_folder {
                                actions
                                    .child(
                                        row_action(
                                            ("row-open-tab", position),
                                            "external",
                                            "Mở trong tab mới",
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _event, window, cx| {
                                                cx.stop_propagation();
                                                this.select_only(position, cx);
                                                this.open_cursor_in_tab(window, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        row_action(
                                            ("row-download", position),
                                            "download",
                                            "Tải xuống",
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _event, _window, cx| {
                                                cx.stop_propagation();
                                                this.select_only(position, cx);
                                                this.download_selection(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        row_action(("row-delete", position), "trash", "Xoá", theme)
                                            .on_click(cx.listener(
                                                move |this, _event, _window, cx| {
                                                    cx.stop_propagation();
                                                    this.select_only(position, cx);
                                                    this.ask_delete_selection(cx);
                                                },
                                            )),
                                    )
                            } else {
                                actions
                                    .child(
                                        row_action(
                                            ("row-info", position),
                                            "info",
                                            "Chi tiết",
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _event, _window, cx| {
                                                cx.stop_propagation();
                                                this.select_only(position, cx);
                                                this.open_inspector(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        row_action(
                                            ("row-preview", position),
                                            "eye",
                                            "Xem trước",
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _event, _window, cx| {
                                                cx.stop_propagation();
                                                this.select_only(position, cx);
                                                this.open_preview(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        row_action(
                                            ("row-open-external", position),
                                            "external",
                                            "Mở bằng app",
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _event, _window, cx| {
                                                cx.stop_propagation();
                                                this.select_only(position, cx);
                                                this.open_externally(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        row_action(
                                            ("row-share", position),
                                            "link",
                                            "Chia sẻ",
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _event, _window, cx| {
                                                cx.stop_propagation();
                                                this.select_only(position, cx);
                                                this.start_share(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        row_action(
                                            ("row-download", position),
                                            "download",
                                            "Tải xuống",
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _event, _window, cx| {
                                                cx.stop_propagation();
                                                this.select_only(position, cx);
                                                this.download_selection(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        row_action(("row-delete", position), "trash", "Xoá", theme)
                                            .on_click(cx.listener(
                                                move |this, _event, _window, cx| {
                                                    cx.stop_propagation();
                                                    this.select_only(position, cx);
                                                    this.ask_delete_selection(cx);
                                                },
                                            )),
                                    )
                            };

                            actions
                                .transition_opacity_and_offset(
                                    SharedString::from(format!("row-actions-in-{position}")),
                                    smooth_transition(MOTION_QUICK_MS),
                                    0.0,
                                    1.0,
                                    motion_offset(5.0, 0.0),
                                    motion_offset(0.0, 0.0),
                                )
                                .into_any_element()
                        } else {
                            div()
                                .w(px(ACTIONS_WIDTH))
                                .flex_shrink_0()
                                .into_any_element()
                        };

                        object_row(
                            entry,
                            RowState {
                                position,
                                selected,
                                cursor: is_cursor,
                                thumbnail,
                            },
                            checkbox,
                            actions,
                            theme,
                        )
                        .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                            if *hovered {
                                if this.hovered_row != Some(position) {
                                    this.hovered_row = Some(position);
                                    cx.notify();
                                }
                            } else if this.hovered_row == Some(position) {
                                this.hovered_row = None;
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                            // gpui 0.2.2 has no `on_double_click`, but the click
                            // event carries the count, so both gestures live here.
                            if click_count(event) >= 2 {
                                // Selecting first means the preview and the
                                // inspector act on the row just opened.
                                this.click_row(position, event.modifiers(), cx);
                                if is_folder {
                                    this.enter(entry_index, cx);
                                } else {
                                    // A double-clicked file used to do
                                    // nothing but select, which no file
                                    // manager does.
                                    this.open_preview(cx);
                                }
                            } else {
                                this.click_row(position, event.modifiers(), cx);
                            }
                        }))
                        .on_mouse_down(
                            gpui::MouseButton::Right,
                            cx.listener(move |this, _event, _window, cx| {
                                if !this.selection.contains(&right_click_key) {
                                    this.select_only(position, cx);
                                }
                            }),
                        )
                        .context_menu(move |menu, _window, _cx| {
                            object_context_menu(menu, single, can_paste, is_folder, acl_supported)
                        })
                        .transition_opacity_and_offset(
                            SharedString::from(format!("object-row-in-{position}")),
                            delayed_transition(
                                MOTION_ROW_MS,
                                row_stagger_from_end(position, range_end),
                            ),
                            0.0,
                            1.0,
                            motion_offset(0.0, -3.0),
                            motion_offset(0.0, 0.0),
                        )
                        .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.scroll.clone())
        .h_full()
    }

    fn render_grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let rows = grid_row_count(self.visible.len());

        uniform_list(
            "objects-grid",
            rows,
            cx.processor(move |this: &mut Self, range: Range<usize>, _window, cx| {
                let object_start = range.start * GRID_COLUMNS;
                let object_end = (range.end * GRID_COLUMNS).min(this.visible.len());
                let object_range = object_start..object_end;
                this.perf.note_visible(&object_range);
                this.maybe_prefetch(&object_range, cx);
                this.load_thumbnails(&object_range, cx);

                range
                    .map(|row| {
                        let start = row * GRID_COLUMNS;
                        let end = (start + GRID_COLUMNS).min(this.visible.len());
                        div()
                            .id(SharedString::from(format!("grid-row-{row}")))
                            .h(px(GRID_ROW_HEIGHT))
                            .w_full()
                            .px_3()
                            .pt_3()
                            .flex()
                            .gap_3()
                            .children((start..end).map(|position| {
                                let entry_index = this.visible[position];
                                let entry = &this.entries[entry_index];
                                let selected = this.selection.contains(&entry.key);
                                let is_cursor = this.cursor.as_deref() == Some(entry.key.as_str());
                                let is_folder = entry.is_folder;
                                let right_click_key = entry.key.clone();
                                let thumbnail = this.thumbnails.get(&entry.key).cloned().flatten();
                                let single = this.selection.len() <= 1;
                                let multi_selection = this.selection.len() > 1;
                                let can_paste = this.clipboard.is_some();
                                let acl_supported = this.supports(|caps| caps.acl);
                                let actions_visible = !multi_selection
                                    && (selected
                                        || is_cursor
                                        || this.hovered_row == Some(position));

                                let checkbox = div()
                                    .id(("grid-check", position))
                                    .size(px(22.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .child(check_box(selected, theme))
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.toggle_row(position, cx);
                                    }))
                                    .into_any_element();

                                let actions = if actions_visible {
                                    let actions = div()
                                        .h(px(22.))
                                        .flex()
                                        .items_center()
                                        .justify_end()
                                        .gap_0p5();
                                    let actions = if is_folder {
                                        actions
                                            .child(
                                                row_action(
                                                    ("grid-open-tab", position),
                                                    "external",
                                                    "Mở trong tab mới",
                                                    theme,
                                                )
                                                .on_click(cx.listener(
                                                    move |this, _event, window, cx| {
                                                        cx.stop_propagation();
                                                        this.select_only(position, cx);
                                                        this.open_cursor_in_tab(window, cx);
                                                    },
                                                )),
                                            )
                                            .child(
                                                row_action(
                                                    ("grid-download", position),
                                                    "download",
                                                    "Tải xuống",
                                                    theme,
                                                )
                                                .on_click(cx.listener(
                                                    move |this, _event, _window, cx| {
                                                        cx.stop_propagation();
                                                        this.select_only(position, cx);
                                                        this.download_selection(cx);
                                                    },
                                                )),
                                            )
                                    } else {
                                        actions
                                            .child(
                                                row_action(
                                                    ("grid-info", position),
                                                    "info",
                                                    "Chi tiết",
                                                    theme,
                                                )
                                                .on_click(cx.listener(
                                                    move |this, _event, _window, cx| {
                                                        cx.stop_propagation();
                                                        this.select_only(position, cx);
                                                        this.open_inspector(cx);
                                                    },
                                                )),
                                            )
                                            .child(
                                                row_action(
                                                    ("grid-preview", position),
                                                    "eye",
                                                    "Xem trước",
                                                    theme,
                                                )
                                                .on_click(cx.listener(
                                                    move |this, _event, _window, cx| {
                                                        cx.stop_propagation();
                                                        this.select_only(position, cx);
                                                        this.open_preview(cx);
                                                    },
                                                )),
                                            )
                                            .child(
                                                row_action(
                                                    ("grid-download", position),
                                                    "download",
                                                    "Tải xuống",
                                                    theme,
                                                )
                                                .on_click(cx.listener(
                                                    move |this, _event, _window, cx| {
                                                        cx.stop_propagation();
                                                        this.select_only(position, cx);
                                                        this.download_selection(cx);
                                                    },
                                                )),
                                            )
                                    };
                                    actions
                                        .transition_opacity_and_offset(
                                            SharedString::from(format!(
                                                "grid-actions-in-{position}"
                                            )),
                                            smooth_transition(MOTION_QUICK_MS),
                                            0.0,
                                            1.0,
                                            motion_offset(0.0, 4.0),
                                            motion_offset(0.0, 0.0),
                                        )
                                        .into_any_element()
                                } else {
                                    div().h(px(22.)).into_any_element()
                                };

                                object_grid_card(
                                    entry,
                                    RowState {
                                        position,
                                        selected,
                                        cursor: is_cursor,
                                        thumbnail,
                                    },
                                    checkbox,
                                    actions,
                                    theme,
                                )
                                .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                                    if *hovered {
                                        if this.hovered_row != Some(position) {
                                            this.hovered_row = Some(position);
                                            cx.notify();
                                        }
                                    } else if this.hovered_row == Some(position) {
                                        this.hovered_row = None;
                                        cx.notify();
                                    }
                                }))
                                .on_click(cx.listener(
                                    move |this, event: &ClickEvent, _window, cx| {
                                        if click_count(event) >= 2 {
                                            this.click_row(position, event.modifiers(), cx);
                                            if is_folder {
                                                this.enter(entry_index, cx);
                                            } else {
                                                this.open_preview(cx);
                                            }
                                        } else {
                                            this.click_row(position, event.modifiers(), cx);
                                        }
                                    },
                                ))
                                .on_mouse_down(
                                    gpui::MouseButton::Right,
                                    cx.listener(move |this, _event, _window, cx| {
                                        if !this.selection.contains(&right_click_key) {
                                            this.select_only(position, cx);
                                        }
                                    }),
                                )
                                .context_menu(move |menu, _window, _cx| {
                                    object_context_menu(
                                        menu,
                                        single,
                                        can_paste,
                                        is_folder,
                                        acl_supported,
                                    )
                                })
                                .transition_opacity_and_offset(
                                    SharedString::from(format!("object-grid-in-{position}")),
                                    delayed_transition(
                                        MOTION_PAGE_MS,
                                        grid_stagger_from_end(position, object_end),
                                    ),
                                    0.0,
                                    1.0,
                                    motion_offset(0.0, 10.0),
                                    motion_offset(0.0, 0.0),
                                )
                                .into_any_element()
                            }))
                            .children(
                                (end - start..GRID_COLUMNS)
                                    .map(|_| div().flex_1().min_w(px(0.)).into_any_element()),
                            )
                            .into_any_element()
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
            // Also blocked while the rest is being fetched for a sort: two
            // readers of the same continuation token would each take a page and
            // neither would see the other's.
            self.loading_more || self.completing.is_some(),
        ) {
            self.load_more(cx);
        }
    }

    /// The chip that says a file is on its way down to be handed to another app.
    ///
    /// Floating above the status bar rather than written into it. "Mở bằng app"
    /// is reached from the preview overlay as often as from the listing, and
    /// that overlay's scrim covers the status bar — a line nobody can see is
    /// not feedback. It is also the only progress in the app that is not a
    /// queue job, so the drawer is not where anyone would look for it.
    fn render_opening(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let opening = self.opening.as_ref()?;
        let theme = self.theme;
        let done = opening.done.load(Ordering::Relaxed);
        let total = opening.total.load(Ordering::Relaxed);
        // Empty rather than full while the size is still unknown: a bar sitting
        // at 100% before anything has arrived is a worse lie than no bar.
        let fraction = if total > 0 {
            (done as f32 / total as f32).clamp(0., 1.)
        } else {
            0.
        };
        let measure = if total > 0 {
            format!("{} / {}", format_size(done as i64), format_size(total as i64))
        } else {
            format_size(done as i64)
        };

        Some(
            div()
                .absolute()
                .bottom(px(HEADER_HEIGHT + 10.))
                .right(px(14.))
                .w(px(OPENING_WIDTH))
                .p_2()
                .flex()
                .flex_col()
                .gap_1p5()
                .rounded_lg()
                .bg(theme.modal)
                .border_1()
                .border_color(theme.border_strong)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(sized_icon("download", 12., theme.text_faint))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_xs()
                                .text_color(theme.text)
                                .child(opening.name.clone()),
                        )
                        // A fetch of a large object is a wait someone may change
                        // their mind about, and the only other way out of it is
                        // to start something else and hope it cancels this.
                        .child(
                            small_icon_button("opening-cancel".into(), "close", 10., theme)
                                .size(px(18.))
                                .tooltip(move |window, cx| {
                                    Tooltip::new("Huỷ").build(window, cx)
                                })
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.cancel_open(cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .h(px(PROGRESS_HEIGHT))
                        .rounded_sm()
                        .bg(theme.hover)
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(fraction))
                                .rounded_sm()
                                .bg(theme.accent),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.text_faint)
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .whitespace_nowrap()
                                .child(SharedString::from(measure)),
                        )
                        .when(total > 0, |this| {
                            this.child(SharedString::from(format!(
                                "{}%",
                                (fraction * 100.0).round() as u32
                            )))
                        }),
                ),
        )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let stats = self.transfers.stats();
        let queue_label = if stats.active + stats.queued > 0 {
            let speed = if stats.bytes_per_second > 0 {
                let eta = stats
                    .seconds_remaining()
                    .map(|secs| format!("  còn {}", format_duration(secs)))
                    .unwrap_or_default();
                format!("  {}/s{eta}", format_size(stats.bytes_per_second as i64))
            } else {
                String::new()
            };
            format!("{} đang chạy, {} chờ{speed}", stats.active, stats.queued)
        } else {
            // Nothing while idle. "24 xong" sat in the bar for the rest of the
            // session after one download, and the sidebar's queue row carries
            // the failure count, which is the only idle state worth a mark.
            String::new()
        };

        div()
            .h(px(HEADER_HEIGHT))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .text_xs()
            .bg(theme.panel)
            .border_t_1()
            .border_color(theme.border)
            // With nothing to report, which account this is. The same path
            // under two profiles is two different places, and that is worth a
            // permanent line rather than a thing to go and check.
            .when(self.status.is_empty() && self.failures.is_empty(), |this| {
                let profile = self.active_profile.and_then(|ix| self.profiles.get(ix));
                this.child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        // The name reads a step stronger than the region: one
                        // is which account, the other is a detail of it, and
                        // two runs of identical faint text side by side read as
                        // one thing with a space in it.
                        .children(profile.map(|profile| {
                            div()
                                .text_color(theme.text_muted)
                                .child(SharedString::from(profile.name.clone()))
                        }))
                        .children(profile.map(|_| status_divider(theme)))
                        .children(profile.map(|profile| {
                            div()
                                .text_color(theme.text_faint)
                                .child(SharedString::from(profile.region.clone()))
                        })),
                )
            })
            .child(match self.failures.last() {
                // Clickable, because the summary is one line and the rest of
                // what went wrong has to be reachable from where it is shown.
                Some(failure) => div()
                    .id("failure-chip")
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(theme.danger)
                    .hover(|this| this.bg(theme.hover))
                    .child(SharedString::from(if self.failures.len() > 1 {
                        format!("{} ({} lỗi)", failure.summary, self.failures.len())
                    } else {
                        failure.summary.to_string()
                    }))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.failures_open = true;
                        cx.notify();
                    }))
                    .into_any_element(),
                None => div()
                    .text_color(theme.text_muted)
                    .child(self.status.clone())
                    .into_any_element(),
            })
            // The message is the one thing here that may be cut: it is a
            // sentence, and half a sentence still says something. Everything to
            // the right is a control or a fact, and half of one of those is
            // worth nothing.
            .child(div().flex_1().min_w(px(0.)))
            .when_some(self.perf.label(), |this, label| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.text_faint)
                        .child(SharedString::from(label)),
                )
                .child(status_divider(theme))
            })
            // A batch of four hundred copies takes long enough that "no way to
            // stop it" is not an acceptable answer.
            .when_some(
                self.bulk.as_ref().filter(|bulk| bulk.running).map(|_| ()),
                |this, ()| {
                    this.child(
                        action_button("bulk-stop", "Dừng", theme)
                            .on_click(cx.listener(|this, _event, _window, cx| this.stop_bulk(cx))),
                    )
                },
            )
            // Same for reading a hundred pages to make a sort exact: stopping
            // leaves what arrived and says the sort is over part of the prefix,
            // which is a worse answer than the whole one and a better answer
            // than a bill nobody expected.
            .when_some(
                self.completing.as_ref().filter(|c| c.running).map(|_| ()),
                |this, ()| {
                    this.child(action_button("complete-stop", "Dừng", theme).on_click(
                        cx.listener(|this, _event, _window, cx| this.stop_completing(cx)),
                    ))
                },
            )
            // One pointer instead of four hints. The old strip listed ⌘F, ⌘N,
            // ⌘D and ⌘J — a cheat sheet that was both permanent and
            // incomplete, taking more of this bar than everything that changes
            // on it put together, and repeating ⌘J directly beside the queue
            // button it names. The palette already prints every command's own
            // shortcut beside it, so one way in covers the lot.
            .child(
                div()
                    .id("status-palette")
                    .flex_shrink_0()
                    .px_1p5()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(theme.text_faint)
                    .hover(|this| this.bg(theme.hover).text_color(theme.text_muted))
                    .child(SharedString::from(format!(
                        "{}K lệnh",
                        platform::primary_modifier()
                    )))
                    .on_click(
                        cx.listener(|this, _event, window, cx| this.open_palette(window, cx)),
                    ),
            )
            .child(status_divider(theme))
            // What the sidebar's queue row cannot hold. That row answers "is
            // there anything in there"; this answers "how fast, and how much
            // longer", which is worth a strip of its own while it is true and
            // worth nothing at all once the queue is idle.
            .when(!queue_label.is_empty(), |this| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.text_muted)
                        .child(SharedString::from(queue_label)),
                )
                .child(status_divider(theme))
            })
            .child(status_divider(theme))
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(theme.text_faint)
                    .child(concat!("v", env!("CARGO_PKG_VERSION"))),
            )
    }

    /// "bộ nhớ tạm", with how far behind it is when that can be worked out.
    ///
    /// Kept short enough to stay on one line of a 214px sidebar: the first
    /// draft read "từ bộ nhớ tạm · dưới 1 phút trước" and wrapped, splitting
    /// "trước" across two lines and pushing the bucket list down a row.
    fn bucket_cache_note(&self) -> String {
        let age = self
            .active_profile
            .and_then(|ix| self.profiles.get(ix))
            .and_then(|profile| self.bucket_cache.get(&profile.id))
            .map(|(_, at)| format_age(s3core::now_epoch() - at));
        match age {
            Some(age) => format!("bộ nhớ tạm · {age}"),
            None => "bộ nhớ tạm".to_string(),
        }
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
                .h(px(PANEL_HEIGHT))
                .flex()
                .flex_col()
                .bg(theme.panel)
                .border_t_1()
                .border_color(theme.border_strong)
                .child(
                    div()
                        .h(px(CONTROL_BAR_HEIGHT))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .text_xs()
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!("HÀNG ĐỢI {}", jobs.len())))
                        .child(div().flex_1())
                        .child(
                            setting_chip(
                                "bandwidth",
                                "Băng thông",
                                bandwidth_label(self.transfers.bandwidth_limit()),
                                theme,
                            )
                            .on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    let next =
                                        next_bandwidth_limit(this.transfers.bandwidth_limit());
                                    this.transfers.set_bandwidth_limit(next);
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            action_button("clear-finished", "Xoá đã xong", theme).on_click(
                                cx.listener(|this, _event, _window, cx| {
                                    this.transfers.clear_finished();
                                    this.prune_expanded_folders();
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
                    // Laid out once and moved into the closure. Building it
                    // inside instead cost four passes a frame, not the two it
                    // looked like: gpui measures an item in `request_layout`,
                    // measures again in `prepaint`, then asks for the visible
                    // range — and each of those took its own `snapshot()` and
                    // regrouped every job. It also closed the window where the
                    // count and the rows came from two different snapshots.
                    let rows = queue_rows(&jobs, &self.expanded_folders);
                    let lines = rows.len();
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(self.render_job_header())
                        .child(
                            uniform_list(
                                "transfers",
                                lines,
                                cx.processor(move |this, range: Range<usize>, _window, cx| {
                                    range
                                        .filter_map(|ix| rows.get(ix).cloned())
                                        .map(|row| this.render_queue_row(row, cx))
                                        .collect::<Vec<_>>()
                                }),
                            )
                            .flex_1(),
                        )
                        .into_any_element()
                })
                .transition_opacity_and_offset(
                    "transfer-drawer-in",
                    smooth_transition(MOTION_PANEL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, 14.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    fn render_inspector(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let inspector = self.inspector.as_ref()?;
        let theme = self.theme;

        let body = match (&inspector.head, inspector.loading) {
            // Same placeholders as the list and the sidebar, so all three areas
            // say "waiting" in one visual language rather than three.
            (None, true) => skeleton_details(theme).into_any_element(),
            (None, false) => div()
                .p_3()
                .text_xs()
                .text_color(theme.text_faint)
                .child("Không đọc được metadata")
                .into_any_element(),
            (Some(head), _) => {
                let class = head.storage_class.as_deref().unwrap_or("STANDARD");
                let state = restore_state(head.restore.as_deref(), head.storage_class.as_deref());
                let acl_target_count = self.selected_object_keys().len();
                let acl_target = acl_target_label(acl_target_count);
                let acl_support = self.acl_support();
                let acl_supported = acl_support.map(Support::is_usable).unwrap_or(true);
                let acl_unavailable = acl_unavailable_copy(acl_support);

                div()
                    .id("inspector-body")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .text_xs()
                    .child(
                        inspector_section("THUỘC TÍNH", theme)
                            .child(detail_row("Kích thước", format_size(head.size), theme))
                            .child(detail_row(
                                "Sửa đổi",
                                head.modified_epoch
                                    .map(format_timestamp)
                                    .unwrap_or_default(),
                                theme,
                            ))
                            .child(detail_row(
                                "Kiểu",
                                head.content_type.clone().unwrap_or_default(),
                                theme,
                            ))
                            // Shown only when set, because on most objects they
                            // are not, and two permanently blank rows push the
                            // ones that say something out of the panel.
                            .when_some(head.cache_control.clone(), |this, value| {
                                this.child(detail_row("Cache", value, theme))
                            })
                            .when_some(head.content_disposition.clone(), |this, value| {
                                this.child(detail_row("Trả về", value, theme))
                            })
                            .child(detail_row("Lớp lưu trữ", class.to_string(), theme))
                            .child(detail_row(
                                "Mã hoá",
                                match (&head.encryption, &head.kms_key_id) {
                                    (Some(kind), Some(key)) => format!("{kind} ({key})"),
                                    (Some(kind), None) => kind.clone(),
                                    // Not the same as "unknown": S3 omits the
                                    // header exactly when nothing encrypts it.
                                    (None, _) => "không".into(),
                                },
                                theme,
                            ))
                            .child(detail_row(
                                "ETag",
                                elide_middle(
                                    &head.etag.clone().unwrap_or_default().replace('"', ""),
                                    28,
                                ),
                                theme,
                            )),
                    )
                    // Next to the headers it edits. These three decide whether a
                    // shared link renders in a tab or lands in the downloads
                    // folder, which is not something to go hunting for.
                    .child(action_button("edit-headers", "Sửa header", theme).on_click(
                        cx.listener(|this, _event, window, cx| this.start_edit_headers(window, cx)),
                    ))
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
                                .child("Đang khôi phục, chưa đọc được")
                                .into_any_element(),
                            _ => div()
                                .text_color(theme.text_muted)
                                .child("Đã khôi phục, đọc được tạm thời")
                                .into_any_element(),
                        })
                    })
                    .when_some(acl_unavailable, |this, (summary, detail)| {
                        this.child(
                            inspector_section("QUYỀN TRUY CẬP", theme)
                                .child(acl_unavailable_notice(summary, detail, theme)),
                        )
                    })
                    .when(acl_supported, |this| {
                        let acl = inspector.acl.clone();
                        let acl_target = acl_target.clone();
                        this.child({
                            let public = acl
                                .as_ref()
                                .is_some_and(|acl| acl.grants.iter().any(|grant| grant.public));
                            let section = inspector_section("QUYỀN TRUY CẬP", theme)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(acl_status_chip(public, theme))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w(px(0.))
                                                .text_color(theme.text_faint)
                                                .child(SharedString::from(acl_target)),
                                        ),
                                );

                            let section = if let Some(acl) = acl {
                                section
                                    .child(detail_row(
                                        "Chủ sở hữu",
                                        elide_middle(&acl.owner, 30),
                                        theme,
                                    ))
                                    .child(
                                        action_button("acl-table", "Bảng quyền", theme).on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.acl_popup_open = true;
                                                cx.notify();
                                            }),
                                        ),
                                    )
                            } else {
                                section.child(
                                    div()
                                        .text_color(theme.text_faint)
                                        .child("Provider không trả ACL đọc được, vẫn có thể thử đặt ACL canned."),
                                )
                            };

                            section
                                .child(div().text_color(theme.text_faint).child("Preset nhanh"))
                                .child(div().flex().flex_wrap().gap_1().children(
                                    s3core::CANNED_ACLS.iter().map(|(canned, label)| {
                                        let canned = *canned;
                                        action_button_dyn(
                                            SharedString::from(format!("acl-{canned}")),
                                            SharedString::from(*label),
                                            theme,
                                        )
                                        .on_click(cx.listener(move |this, _event, _window, cx| {
                                            this.set_acl(canned, cx)
                                        }))
                                    }),
                                ))
                        })
                    })
                    .when(self.supports(|caps| caps.tagging), |this| {
                        this.child(
                            inspector_section("THẺ", theme)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_end()
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
                                            div().flex_1().text_color(theme.text).child(
                                                SharedString::from(format!("{name} = {value}")),
                                            ),
                                        )
                                        .child(
                                            icon_button_dyn(
                                                SharedString::from(format!("rm-tag-{name}")),
                                                "close",
                                                theme,
                                            )
                                            .on_click(
                                                cx.listener(move |this, _event, _window, cx| {
                                                    this.remove_tag(removing.clone(), cx)
                                                }),
                                            ),
                                        )
                                }))
                                .when(inspector.tags.is_empty(), |this| {
                                    this.child(
                                        div().text_color(theme.text_faint).child("Chưa có thẻ nào"),
                                    )
                                }),
                        )
                    })
                    .when(
                        !inspector.versions.is_empty() && self.supports(|caps| caps.versioning),
                        |this| {
                            this.child(
                                inspector_section(
                                    SharedString::from(format!(
                                        "PHIÊN BẢN {}",
                                        inspector.versions.len()
                                    )),
                                    theme,
                                )
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
                                                    .child(SharedString::from(
                                                        if version.is_delete_marker {
                                                            format!("{when}   delete marker")
                                                        } else {
                                                            format!(
                                                                "{when}   {}{}",
                                                                format_size(version.size),
                                                                if version.is_latest {
                                                                    "   hiện tại"
                                                                } else {
                                                                    ""
                                                                }
                                                            )
                                                        },
                                                    )),
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
                                                        this.ask_delete_version(
                                                            for_delete.clone(),
                                                            cx,
                                                        )
                                                    },
                                                )),
                                            )
                                    })),
                            )
                        },
                    )
                    .when(!head.metadata.is_empty(), |this| {
                        this.child(
                            inspector_section("METADATA", theme)
                                .children(head.metadata.iter().map(|(name, value)| {
                                    div()
                                        .text_color(theme.text)
                                        .child(SharedString::from(format!("{name} = {value}")))
                                })),
                        )
                    })
                    .into_any_element()
            }
        };
        let selected_object_count = self.selected_object_keys().len();
        let detail_is_multi = selected_object_count > 1;
        let inspected_name = entry_name_of(&inspector.key);
        let detail_title: SharedString = if detail_is_multi {
            format!("{selected_object_count} tệp đã chọn").into()
        } else {
            SharedString::from(inspected_name.clone())
        };
        let detail_subtitle = detail_is_multi.then(|| {
            SharedString::from(format!("Đang xem: {}", elide_middle(&inspected_name, 28)))
        });
        // Built at the render site rather than folded into one string here:
        // the two cases are different shapes, not two values of one shape.
        let inspector_kind = if detail_is_multi {
            count_tile(selected_object_count, theme)
        } else {
            kind_tile(type_badge_of(&inspector.key), KIND_TILE, theme)
        };

        Some(
            div()
                .id("inspector-panel")
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(INSPECTOR_WIDTH))
                // Never squeezed. It is a panel someone opened on purpose, and
                // half of one is worse than none: the labels survive a squeeze
                // and the values are what they opened it for.
                .flex_shrink_0()
                .h_full()
                .flex()
                .flex_col()
                .bg(theme.modal)
                .border_1()
                .border_color(theme.border)
                .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation())
                .on_click(|_event, _window, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex_shrink_0()
                        .px_3()
                        .py_2()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(inspector_kind)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .flex()
                                        .flex_col()
                                        .gap_0p5()
                                        .child(
                                            ellipsis_text(
                                                SharedString::from(format!(
                                                    "detail-title-{}",
                                                    animation_key_hash(&inspector.key)
                                                )),
                                                detail_title.to_string(),
                                                34,
                                                theme.text,
                                            )
                                            .text_xs(),
                                        )
                                        .when_some(detail_subtitle, |this, subtitle| {
                                            this.child(
                                                ellipsis_text(
                                                    SharedString::from(format!(
                                                        "detail-subtitle-{}",
                                                        animation_key_hash(&inspector.key)
                                                    )),
                                                    subtitle.to_string(),
                                                    34,
                                                    theme.text_faint,
                                                )
                                                .text_xs(),
                                            )
                                        }),
                                )
                                .child(icon_button("close-inspector", "close", theme).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.inspector = None;
                                        cx.notify();
                                    }),
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_1()
                                .when(!detail_is_multi, |this| {
                                    this.child(
                                        detail_action_button("detail-preview", "Xem", "eye", theme)
                                            .on_click(cx.listener(|this, _event, _window, cx| {
                                                this.open_preview(cx)
                                            })),
                                    )
                                })
                                .child(
                                    detail_action_button(
                                        "detail-edit-headers",
                                        "Headers",
                                        "rename",
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, window, cx| {
                                            if detail_is_multi {
                                                this.edit_headers_for_selection(window, cx);
                                            } else {
                                                this.start_edit_headers(window, cx);
                                            }
                                        },
                                    )),
                                )
                                .when(!detail_is_multi, |this| {
                                    this.child(
                                        detail_action_button(
                                            "detail-share",
                                            "Chia sẻ",
                                            "link",
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.start_share_current(cx)
                                            }),
                                        ),
                                    )
                                })
                                .child(
                                    detail_action_button(
                                        "detail-download",
                                        "Tải xuống",
                                        "download",
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _event, _window, cx| {
                                            if detail_is_multi {
                                                this.download_selection(cx);
                                            } else {
                                                this.download_current_detail(cx);
                                            }
                                        },
                                    )),
                                )
                                .when(!detail_is_multi, |this| {
                                    this.child(
                                        detail_action_button(
                                            "detail-open-external",
                                            "Mở app",
                                            "external",
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.open_externally(cx)
                                            }),
                                        ),
                                    )
                                }),
                        ),
                )
                .child(body)
                .transition_opacity_and_offset(
                    "inspector-panel-in",
                    smooth_transition(MOTION_PANEL_MS),
                    0.0,
                    1.0,
                    motion_offset(16.0, 0.0),
                    motion_offset(0.0, 0.0),
                ),
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
                        .w(px(DIALOG_WIDTH))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_xl()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(
                            div()
                                .text_color(theme.text)
                                .child(SharedString::from(format!("Chia sẻ {name}"))),
                        )
                        .child(div().flex().gap_2().children(PRESIGN_PRESETS.iter().map(
                            |(label, expires)| {
                                let expires = *expires;
                                let label = *label;
                                let active = share.chosen == label;
                                action_button_dyn(
                                    SharedString::from(format!("presign-{label}")),
                                    SharedString::from(label),
                                    theme,
                                )
                                // The active preset has to look chosen, or
                                // the row is four buttons with no state.
                                .when(active, |this| this.bg(theme.selected))
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        this.presign(label, expires, cx)
                                    },
                                ))
                            },
                        )))
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
                                // Elided: it is copied, not read, and the full
                                // signature would push the buttons off screen.
                                .child(SharedString::from(elide_middle(url, 52)))
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
                                                this.copy_to_clipboard(url.to_string(), "link", cx)
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
                )
                .transition_opacity_and_offset(
                    "share-scrim-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, -8.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    fn render_acl_popup(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.acl_popup_open {
            return None;
        }
        let inspector = self.inspector.as_ref()?;
        let theme = self.theme;
        let selected_object_count = self.selected_object_keys().len();
        let detail_is_multi = selected_object_count > 1;
        let title = if detail_is_multi {
            format!("Quyền truy cập của {selected_object_count} tệp")
        } else {
            format!("Quyền truy cập {}", entry_name_of(&inspector.key))
        };
        let target = acl_target_label(selected_object_count);

        Some(
            div()
                .id("acl-popup-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::hsla(0., 0., 0., 0.34))
                .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.acl_popup_open = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("acl-popup")
                        .w(px(560.))
                        .max_h(px(560.))
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .text_xs()
                        .rounded_xl()
                        .bg(preview_shell(theme))
                        .border_1()
                        .border_color(theme.border_strong)
                        .on_scroll_wheel(|_event, _window, cx| cx.stop_propagation())
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .flex()
                                        .flex_col()
                                        .gap_0p5()
                                        .child(
                                            ellipsis_text(
                                                SharedString::from(format!(
                                                    "acl-popup-title-{}",
                                                    animation_key_hash(&inspector.key)
                                                )),
                                                title,
                                                48,
                                                theme.text,
                                            )
                                            .text_sm(),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.text_faint)
                                                .child(SharedString::from(target)),
                                        ),
                                )
                                .child(icon_button("close-acl-popup", "close", theme).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.acl_popup_open = false;
                                        cx.notify();
                                    }),
                                )),
                        )
                        .child(
                            div()
                                .id("acl-popup-body")
                                .flex_1()
                                .min_h(px(0.))
                                .overflow_scroll()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .when_some(inspector.acl.clone(), |this, acl| {
                                    this.child(acl_permission_table(&acl, theme))
                                })
                                .when(inspector.acl.is_none(), |this| {
                                    this.child(div().text_color(theme.text_faint).child(
                                        if inspector.loading {
                                            "Đang đọc ACL…"
                                        } else {
                                            "Provider không trả ACL đọc được."
                                        },
                                    ))
                                })
                                .child(div().text_color(theme.text_faint).child("Preset nhanh"))
                                .child(div().flex().flex_wrap().gap_1().children(
                                    s3core::CANNED_ACLS.iter().map(|(canned, label)| {
                                        let canned = *canned;
                                        action_button_dyn(
                                            SharedString::from(format!("acl-popup-{canned}")),
                                            SharedString::from(*label),
                                            theme,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _event, _window, cx| {
                                                this.set_acl(canned, cx)
                                            }),
                                        )
                                    }),
                                )),
                        ),
                )
                .transition_opacity_and_offset(
                    "acl-popup-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, 10.0),
                    motion_offset(0.0, 0.0),
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

        let step = |title: &'static str,
                    detail: &'static str,
                    button: &'static str,
                    id: &'static str| { (title, detail, button, id) };

        Some(
            div()
                .id("onboarding")
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(DIALOG_WIDTH))
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
                                step(
                                    "Nhập thủ công",
                                    "R2, B2, Wasabi, Spaces, MinIO",
                                    "Tạo",
                                    "onboard-manual",
                                ),
                                step("MinIO trên máy", "127.0.0.1:9000", "Tạo", "onboard-minio"),
                                step(
                                    "Đăng nhập AWS SSO",
                                    "Qua trình duyệt",
                                    "Đăng nhập",
                                    "onboard-sso",
                                ),
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
                                            "onboard-manual" => {
                                                this.open_form(FormKind::NewProfile, window, cx)
                                            }
                                            "onboard-minio" => this.add_minio_dev_profile(cx),
                                            _ => this.open_form(FormKind::SsoStart, window, cx),
                                        },
                                    )))
                            }),
                        )
                        .when_some(
                            self.failures.last().map(|failure| failure.summary.clone()),
                            |this, summary| {
                                this.child(div().text_xs().text_color(theme.danger).child(summary))
                            },
                        ),
                )
                .transition_opacity_and_offset(
                    "onboarding-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, 8.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    fn render_form(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let form = self.form.as_ref()?;
        let theme = self.theme;
        let is_profile = matches!(form.kind, FormKind::NewProfile | FormKind::EditProfile(_));
        let label_width = form_label_width(&form.kind);

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
                        .w(px(if is_profile {
                            PROFILE_DIALOG_WIDTH
                        } else {
                            DIALOG_WIDTH
                        }))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_xl()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(div().text_color(theme.text).child(form.kind.title()))
                        // The preset comes first, because it decides what two
                        // of the fields below should say. Nobody remembers that
                        // R2 wants region `auto` and a hostname built from an
                        // account id, and getting it wrong looks exactly like a
                        // wrong secret key.
                        .when_some(
                            form.provider_select.clone().filter(|_| is_profile),
                            |this, select| {
                                this.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(label_width))
                                                .text_xs()
                                                .text_color(theme.text_faint)
                                                .child("Dịch vụ"),
                                        )
                                        .child(div().flex_1().min_w(px(0.)).child(
                                            Select::new(&select).placeholder("Chọn dịch vụ"),
                                        )),
                                )
                            },
                        )
                        .children(form.fields.iter().map(|field| {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(label_width))
                                        .flex_shrink_0()
                                        .text_xs()
                                        .text_color(theme.text_faint)
                                        .child(field.label),
                                )
                                // The real thing: cursor, selection, ⌘A, ⌘V,
                                // undo and IME are the widget's, not ours.
                                .child(div().flex_1().min_w(px(0.)).child(
                                    Input::new(&field.state).when(
                                        field.masked,
                                        // A secret typed blind cannot be
                                        // checked for a typo, and a wrong
                                        // secret fails identically to a
                                        // wrong endpoint. The eye is how
                                        // that gets ruled out.
                                        |input| input.mask_toggle(),
                                    ),
                                ))
                        }))
                        .when_some(form.error.clone(), |this, error| {
                            this.child(div().text_xs().text_color(theme.danger).child(error))
                        })
                        // Its own row, under the credentials it tests and well
                        // away from Huỷ/Lưu. Sitting in that group it read as a
                        // third way to close the dialog, which is the one thing
                        // it is not: it changes nothing and saves nothing.
                        .when(is_profile, |this| {
                            this.child(
                                div()
                                    .pt_2()
                                    .border_t_1()
                                    .border_color(theme.border)
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        action_button("form-test", "Thử kết nối", theme).on_click(
                                            cx.listener(|this, _event, _window, cx| {
                                                this.test_profile_connection(cx)
                                            }),
                                        ),
                                    )
                                    .when_some(form.probe.clone(), |this, probe| {
                                        let (colour, text) = match probe {
                                            Probe::Running => {
                                                (theme.text_muted, "Đang thử…".into())
                                            }
                                            Probe::Ok(message) => (theme.accent, message),
                                            Probe::Warning(message) => (theme.text_muted, message),
                                            Probe::Failed(message) => (theme.danger, message),
                                        };
                                        this.child(div().text_xs().text_color(colour).child(text))
                                    }),
                            )
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
                                    cx.listener(|this, _event, _window, cx| this.submit_form(cx)),
                                )),
                        ),
                )
                .transition_opacity_and_offset(
                    "form-scrim-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, -8.0),
                    motion_offset(0.0, 0.0),
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
                        .w(px(DIALOG_WIDTH))
                        .max_h(px(420.))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_xl()
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
                        .child(div().flex().justify_end().child(
                            action_button("sso-cancel", "Huỷ", theme).on_click(cx.listener(
                                |this, _event, _window, cx| {
                                    this.sso = None;
                                    cx.notify();
                                },
                            )),
                        )),
                )
                .transition_opacity_and_offset(
                    "sso-scrim-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, -8.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let query = self.palette.as_ref()?;
        let theme = self.theme;
        let matches = self.palette_matches();

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
                        .w(px(PALETTE_WIDTH))
                        .h(px(PALETTE_HEIGHT))
                        .flex()
                        .flex_col()
                        .rounded_xl()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        // The search line, drawn as a search line: icon, text,
                        // a caret. It used to be a bare string jammed into the
                        // pane's top edge, which read as a stray label rather
                        // than as somewhere typing goes.
                        .child(
                            div()
                                .h(px(48.))
                                .flex_shrink_0()
                                .px_4()
                                .flex()
                                .items_center()
                                .gap_2()
                                .border_b_1()
                                .border_color(theme.border)
                                .child(sized_icon("search", 14., theme.text_faint))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(if query.is_empty() {
                                            theme.text_faint
                                        } else {
                                            theme.text
                                        })
                                        .child(SharedString::from(if query.is_empty() {
                                            "Gõ để tìm lệnh…".to_string()
                                        } else {
                                            query.clone()
                                        })),
                                )
                                // A standing caret. The palette really does
                                // take keystrokes, and nothing else on this
                                // line says so.
                                .child(div().w(px(1.5)).h(px(16.)).bg(theme.accent)),
                        )
                        .child(
                            div()
                                .id("palette-list")
                                .flex_1()
                                .min_h(px(0.))
                                .px_2()
                                .py_2()
                                .when(matches.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .flex_1()
                                            .h_full()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_sm()
                                            .text_color(theme.text_faint)
                                            .child("Không có lệnh nào khớp"),
                                    )
                                })
                                // A uniform list rather than a scrolling div,
                                // so the same `scroll_to_item` that follows the
                                // cursor in the object list follows the
                                // highlight here. The div scrolled, but nothing
                                // could tell it where to go, so arrowing past
                                // the fold walked the highlight off screen.
                                .when(!matches.is_empty(), |this| {
                                    this.child(
                                        uniform_list(
                                            "palette-rows",
                                            matches.len(),
                                            cx.processor(
                                                move |this: &mut Self,
                                                      range: Range<usize>,
                                                      _window,
                                                      cx| {
                                                    let matches = this.palette_matches();
                                                    let selected = this.palette_selected;
                                                    let theme = this.theme;
                                                    let range_end = range.end;
                                                    range
                                                        .filter_map(|ix| {
                                                            let command = *matches.get(ix)?;
                                                            Some(
                                                                palette_row(
                                                                    ix,
                                                                    command,
                                                                    ix == selected,
                                                                    theme,
                                                                )
                                                                .on_click(cx.listener(
                                                                    move |this, _event, window, cx| {
                                                                        this.run_command(
                                                                            command,
                                                                            Some(window),
                                                                            cx,
                                                                        )
                                                                    },
                                                                ))
                                                                .transition_opacity_and_offset(
                                                                    SharedString::from(format!(
                                                                        "palette-row-in-{ix}"
                                                                    )),
                                                                    delayed_transition(
                                                                        MOTION_ROW_MS,
                                                                        row_stagger_from_end(
                                                                            ix, range_end,
                                                                        ),
                                                                    ),
                                                                    0.0,
                                                                    1.0,
                                                                    motion_offset(0.0, -3.0),
                                                                    motion_offset(0.0, 0.0),
                                                                ),
                                                            )
                                                        })
                                                        .collect::<Vec<_>>()
                                                },
                                            ),
                                        )
                                        .track_scroll(self.palette_scroll.clone())
                                        .h_full(),
                                    )
                                }),
                        )
                        // The footer earns its row by teaching the three keys
                        // the palette lives on — the count rides along because
                        // "no result" and "38 commands" are the two ends of
                        // the same fact.
                        .child(
                            div()
                                .h(px(34.))
                                .flex_shrink_0()
                                .px_4()
                                .flex()
                                .items_center()
                                .justify_between()
                                .border_t_1()
                                .border_color(theme.border)
                                .text_xs()
                                .text_color(theme.text_faint)
                                .child("↑↓ chọn   ↵ chạy   esc đóng")
                                .child(SharedString::from(format!("{} lệnh", matches.len()))),
                        ),
                )
                .transition_opacity_and_offset(
                    "palette-scrim-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, -10.0),
                    motion_offset(0.0, 0.0),
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
                        .w(px(DIALOG_WIDTH))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .rounded_xl()
                        .bg(theme.modal)
                        .border_1()
                        .border_color(theme.border_strong)
                        // Clicks inside the dialog must not reach the scrim.
                        .on_click(|_event, _window, cx| cx.stop_propagation())
                        .child(div().text_color(theme.text).child(confirm.title.clone()))
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
                                .child(danger_button("confirm-ok", "Xoá".into(), theme).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.commit_confirm(cx)
                                    }),
                                )),
                        ),
                )
                .transition_opacity_and_offset(
                    "confirm-scrim-in",
                    smooth_transition(MOTION_MODAL_MS),
                    0.0,
                    1.0,
                    motion_offset(0.0, -8.0),
                    motion_offset(0.0, 0.0),
                ),
        )
    }

    /// Column widths for the transfer table. Shared by the header and the rows
    /// so they line up — the object list taught that lesson the hard way.
    const JOB_COLS: (f32, f32, f32, f32, f32) = (18., 150., 130., 78., 112.);

    fn render_job_header(&self) -> impl IntoElement {
        let theme = self.theme;
        let (icon_w, size_w, progress_w, state_w, actions_w) = Self::JOB_COLS;

        let cell = move |label: &'static str, width: f32| {
            div()
                .w(px(width))
                .text_xs()
                .text_color(theme.text_faint)
                .child(label)
        };

        div()
            .h(px(HEADER_HEIGHT))
            .w_full()
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(div().w(px(icon_w)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_xs()
                    .text_color(theme.text_faint)
                    .child("Tên"),
            )
            .child(cell("Kích thước", size_w))
            .child(cell("Tiến trình", progress_w))
            .child(cell("Trạng thái", state_w))
            // Matches the two action buttons on a row.
            .child(div().w(px(actions_w)))
    }

    fn render_queue_row(&self, row: QueueRow, cx: &mut Context<Self>) -> gpui::AnyElement {
        match row {
            QueueRow::Heading(section) => self.render_queue_heading(section).into_any_element(),
            QueueRow::Folder(folder) => self.render_folder(folder, cx).into_any_element(),
            QueueRow::Job { job, child } => self.render_job(job, child, cx).into_any_element(),
        }
    }

    /// A status heading inside the queue.
    ///
    /// As tall as a job row, not `HEADER_HEIGHT`, because `uniform_list`
    /// measures one row and spaces every other row by that height. A heading
    /// declaring a shorter height would not pull the rows under it up — and
    /// since the first line is always a heading, it is the row that gets
    /// measured, so every file row would be squashed into 28px.
    fn render_queue_heading(&self, section: QueueSection) -> impl IntoElement {
        let theme = self.theme;

        div()
            .h(px(JOB_ROW_HEIGHT))
            .w_full()
            .px_3()
            .flex()
            .items_center()
            .text_xs()
            .text_color(theme.text_faint)
            .child(section.label())
    }

    /// A folder's files on one line, with the same columns as a job row so the
    /// two line up under one header.
    fn render_folder(&self, folder: FolderRow, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let (icon_w, size_w, progress_w, state_w, actions_w) = Self::JOB_COLS;
        let fraction = folder.fraction();
        let state_color = match folder.section {
            QueueSection::Active => theme.accent,
            QueueSection::Done => theme.text_muted,
            QueueSection::Failed => theme.danger,
            QueueSection::Canceled => theme.text_faint,
        };
        let key = folder.key.clone();
        let pause_ids = folder.ids.clone();
        let resume_ids = folder.ids.clone();
        let remove_ids = folder.ids.clone();

        div()
            .id(SharedString::from(format!("folder-{}", folder.key)))
            .h(px(JOB_ROW_HEIGHT))
            .w_full()
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .text_xs()
            .cursor_pointer()
            .hover(|this| this.bg(theme.hover))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                if !this.expanded_folders.remove(&key) {
                    this.expanded_folders.insert(key.clone());
                }
                cx.notify();
            }))
            .child(
                // Pointing where the click sends things, which is the
                // convention the status bar's own queue toggle already set:
                // ▾ while shut, because the files come *down* out of it, and ▴
                // once they are out. The first draft had it the other way and
                // read as "collapse" on a row that was already collapsed.
                div().w(px(icon_w)).child(sized_icon(
                    if folder.expanded {
                        "chevron-up"
                    } else {
                        "chevron-down"
                    },
                    12.,
                    theme.text_faint,
                )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon("folder", theme.accent))
                    .child(
                        div()
                            .text_color(theme.text)
                            .child(SharedString::from(folder.name.clone())),
                    )
                    // The count is what says this row stands for more than one
                    // thing; without it a folder row reads as a single file.
                    .child(
                        div()
                            .text_color(theme.text_faint)
                            .child(SharedString::from(format!("{} tệp", folder.count))),
                    ),
            )
            .child(
                div()
                    .w(px(size_w))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!(
                        "{} / {}",
                        format_size(folder.transferred as i64),
                        format_size(folder.size as i64)
                    ))),
            )
            .child(
                div()
                    .w(px(progress_w))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h(px(PROGRESS_HEIGHT))
                            .rounded_sm()
                            .bg(theme.hover)
                            .child(div().h_full().w(gpui::relative(fraction)).rounded_sm().bg(
                                if folder.section == QueueSection::Failed {
                                    theme.danger
                                } else {
                                    theme.accent
                                },
                            )),
                    )
                    .child(div().w(px(36.)).text_color(theme.text_faint).child(
                        SharedString::from(format!("{}%", (fraction * 100.0).round() as u32)),
                    )),
            )
            .child(
                div()
                    .w(px(state_w))
                    .text_color(state_color)
                    .child(folder.state_label),
            )
            // The buttons act on every file under the row. Pausing two hundred
            // files one at a time is the thing this row exists to avoid.
            .child(
                div()
                    .w(px(actions_w))
                    .flex()
                    .justify_end()
                    .gap_1()
                    // Both, when both apply. Unlike a single job the states
                    // here are not exclusive — one queued file and one failed
                    // file makes a folder that can be paused *and* resumed —
                    // and an `else if` meant the failed one could only be
                    // retried by opening the row.
                    .when(folder.pausable, |this| {
                        this.child(
                            icon_button_dyn(
                                SharedString::from(format!("folder-pause-{}", folder.key)),
                                "pause",
                                theme,
                            )
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    // Or the row underneath fires too and pausing
                                    // the folder also opens it.
                                    cx.stop_propagation();
                                    for id in &pause_ids {
                                        this.transfers.pause(*id);
                                    }
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .when(folder.resumable, |this| {
                        this.child(
                            icon_button_dyn(
                                SharedString::from(format!("folder-resume-{}", folder.key)),
                                "play",
                                theme,
                            )
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    cx.stop_propagation();
                                    if let Some(client) = this.client.clone() {
                                        for id in &resume_ids {
                                            this.transfers.resume(*id, client.clone());
                                        }
                                        this.start_ticking(cx);
                                    }
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .child(
                        icon_button_dyn(
                            SharedString::from(format!("folder-remove-{}", folder.key)),
                            "close",
                            theme,
                        )
                        .on_click(cx.listener(
                            move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                for id in &remove_ids {
                                    this.transfers.remove_job(*id);
                                }
                                this.prune_expanded_folders();
                                cx.notify();
                            },
                        )),
                    ),
            )
    }

    /// Forgets folders that no longer have jobs behind them.
    ///
    /// The open-set is keyed by prefix, so without this a folder uploaded,
    /// cleared, and uploaded again next week comes back already open — a row
    /// nobody opened, holding files nobody asked to see. Called wherever jobs
    /// leave the queue, which is the only way a key goes stale.
    fn prune_expanded_folders(&mut self) {
        let live: HashSet<String> = self
            .transfers
            .snapshot()
            .iter()
            .filter_map(folder_group_key)
            .collect();
        self.expanded_folders.retain(|key| live.contains(key));
    }

    /// One file. `child` indents it under the folder row it belongs to.
    fn render_job(&self, job: Job, child: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let id = job.id;
        let fraction = job.fraction();
        let (icon_w, size_w, progress_w, state_w, actions_w) = Self::JOB_COLS;
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
            .h(px(JOB_ROW_HEIGHT))
            // Same reason as the object rows: inside a uniform_list a row gets
            // no width from its parent, so flex_1 has nothing to expand into
            // and every column bunches up against the name.
            .w_full()
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .text_xs()
            // Indent, so a file under an open folder row is read as belonging
            // to it rather than as another folder's worth of work.
            .when(child, |this| this.child(div().w(px(14.))))
            .child(div().w(px(icon_w)).child(icon(direction, theme.text_faint)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .text_color(theme.text)
                    .child(SharedString::from(job.display_name())),
            )
            .child(
                div()
                    .w(px(size_w))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!(
                        "{} / {}",
                        format_size(job.transferred as i64),
                        format_size(job.size as i64)
                    ))),
            )
            // Progress: a track with a fill, plus the percentage beside it so
            // the number is readable when the bar is only a few pixels along.
            .child(
                div()
                    .w(px(progress_w))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h(px(PROGRESS_HEIGHT))
                            .rounded_sm()
                            .bg(theme.hover)
                            .child(div().h_full().w(gpui::relative(fraction)).rounded_sm().bg(
                                if job.state == JobState::Failed {
                                    theme.danger
                                } else {
                                    theme.accent
                                },
                            )),
                    )
                    .child(div().w(px(36.)).text_color(theme.text_faint).child(
                        SharedString::from(format!("{}%", (fraction * 100.0).round() as u32)),
                    )),
            )
            .child(
                div()
                    .w(px(state_w))
                    .text_color(state_color)
                    .child(state_label),
            )
            .child(
                div()
                    .w(px(actions_w))
                    .flex()
                    .justify_end()
                    .gap_1()
                    .child(match job.state {
                        JobState::Running | JobState::Queued => icon_button_dyn(
                            SharedString::from(format!("pause-{id}")),
                            "pause",
                            theme,
                        )
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.transfers.pause(id);
                            cx.notify();
                        }))
                        .into_any_element(),
                        JobState::Paused | JobState::Failed => icon_button_dyn(
                            SharedString::from(format!("resume-{id}")),
                            "play",
                            theme,
                        )
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
                        icon_button_dyn(SharedString::from(format!("remove-{id}")), "close", theme)
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.transfers.remove_job(id);
                                cx.notify();
                            })),
                    ),
            )
    }
}

impl Render for Browser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.perf.note_render();
        let theme = self.theme;

        sync_component_theme(theme_mode(self.settings.theme, self.appearance), window, cx);

        // The one place that has both a window and the current location. Not
        // while it has focus: overwriting what someone is halfway through
        // typing is worse than a box that lags a navigation they did not make.
        if self.path_dirty && !self.path_focused(window, cx) {
            self.path_dirty = false;
            if let Some(input) = self.path_input.clone() {
                let text = match self.bucket.as_ref() {
                    Some(bucket) => format!("s3://{bucket}/{}", self.prefix),
                    None => String::new(),
                };
                input.update(cx, |input, cx| input.set_value(text, window, cx));
            }
        }

        div()
            .id("root")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_action(cx.listener(|this, _: &ActionCopy, _window, cx| {
                this.copy_to_clipboard_selection(false, cx)
            }))
            .on_action(cx.listener(|this, _: &ActionCut, _window, cx| {
                this.copy_to_clipboard_selection(true, cx)
            }))
            .on_action(cx.listener(|this, _: &ActionPaste, _window, cx| this.paste(cx)))
            .on_action(
                cx.listener(|this, _: &ActionRename, window, cx| this.start_rename(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActionDuplicate, window, cx| {
                    this.start_duplicate(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ActionDelete, _window, cx| this.ask_delete_selection(cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActionDownload, _window, cx| this.download_selection(cx)),
            )
            .on_action(cx.listener(|this, _: &ActionShare, _window, cx| this.start_share(cx)))
            .on_action(cx.listener(|this, _: &ActionInspect, _window, cx| this.open_inspector(cx)))
            .on_action(cx.listener(|this, _: &ActionSelectAll, _window, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &ActionRefresh, _window, cx| {
                if let (Some(bucket), prefix) = (this.bucket.clone(), this.prefix.clone()) {
                    this.open(bucket, prefix, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ActionNewFolder, window, cx| {
                this.open_form(FormKind::NewFolder, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActionPreview, _window, cx| this.quick_look(cx)))
            .on_action(
                cx.listener(|this, _: &ActionOpenExternally, _window, cx| this.open_externally(cx)),
            )
            .on_action(cx.listener(|this, _: &ActionEditHeaders, window, cx| {
                this.edit_headers_for_selection(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActionOpenInTab, window, cx| {
                this.open_cursor_in_tab(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ActionCopyPath, _window, cx| this.copy_location(true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActionCopyKey, _window, cx| this.copy_location(false, cx)),
            )
            .on_action(cx.listener(|this, _: &ActionAclPrivate, _window, cx| {
                this.set_acl_recursive("private", cx)
            }))
            .on_action(cx.listener(|this, _: &ActionAclPublic, _window, cx| {
                this.set_acl_recursive("public-read", cx)
            }))
            .size_full()
            .flex()
            .flex_col()
            .font_family(self.ui_font.clone())
            .bg(theme.ground)
            .text_color(theme.text)
            // Nothing to browse without a profile, and the chrome behind a
            // translucent overlay read as clutter rather than as background.
            .when_some(self.render_onboarding(cx), |this, onboarding| {
                this.child(onboarding)
            })
            .when(!self.profiles.is_empty(), |this| {
                this.child(self.render_title_bar(cx))
                    .child(self.render_tabs(cx))
                    .child(
                        div()
                            .flex_1()
                            .relative()
                            .flex()
                            .overflow_hidden()
                            .child(self.render_sidebar(cx))
                            // The history page takes the whole pane, and takes the drop
                            // target with it: a file dropped on a list of *past*
                            // locations has no obvious destination, and the honest
                            // answers — the current prefix, or the row under the
                            // cursor — are both things nobody asked for.
                            .when(self.screen == Screen::Recent, |this| {
                                this.child(self.render_recent_page(cx))
                            })
                            .when(self.screen == Screen::Buckets, |this| {
                                this.child(self.render_buckets_page(cx))
                            })
                            .when(self.screen == Screen::Objects, |this| {
                                this.child(
                                    div()
                                        .id("object-pane")
                                        .flex_1()
                                        // A flex child's floor is its content unless it is
                                        // told otherwise, and this one's content is a
                                        // toolbar full of fixed-width buttons. Without
                                        // these two the pane refused to go below about
                                        // 700px and shoved the inspector off the right edge
                                        // of the window — labels still on screen, every
                                        // value gone.
                                        .min_w(px(0.))
                                        .overflow_hidden()
                                        .h_full()
                                        .flex()
                                        .flex_col()
                                        .drag_over::<ExternalPaths>(
                                            move |style, _paths, _window, _cx| {
                                                style.bg(theme.drop_target)
                                            },
                                        )
                                        .on_drop::<ExternalPaths>(cx.listener(
                                            |this, paths: &ExternalPaths, _window, cx| {
                                                this.start_uploads(paths.paths().to_vec(), cx);
                                            },
                                        ))
                                        .when_some(self.render_empty_state(cx), |this, empty| {
                                            this.child(empty)
                                        })
                                        // `connecting` too: the pane is waiting on the same
                                        // request the sidebar is, and leaving it blank until
                                        // a bucket exists reads as a broken window rather
                                        // than as a wait.
                                        .children(self.render_search_bar(cx))
                                        .child(self.render_toolbar(cx))
                                        .when(self.bucket.is_some() || self.connecting, |this| {
                                            this.child(self.render_columns(cx))
                                                .child(self.render_listing(cx))
                                        })
                                        // Outside `render_listing` on purpose: the count
                                        // belongs to the pane, not to whichever of the
                                        // three states the list happens to be in.
                                        .when(self.bucket.is_some() && !self.loading, |this| {
                                            this.child(self.render_list_footer())
                                        })
                                        .transition_opacity_and_offset(
                                            "object-pane-in",
                                            smooth_transition(MOTION_PAGE_MS),
                                            0.0,
                                            1.0,
                                            motion_offset(0.0, 4.0),
                                            motion_offset(0.0, 0.0),
                                        ),
                                )
                            })
                            .when(self.screen == Screen::Objects, |this| {
                                this.children(self.render_inspector(cx))
                            }),
                    )
                    .children(self.render_drawer(cx))
            })
            .child(self.render_status(cx))
            .children(self.render_confirm(cx))
            .children(self.render_share(cx))
            .children(self.render_sso(cx))
            .children(self.render_form(cx))
            .children(self.render_profiles_dialog(cx))
            .children(self.render_failures(cx))
            .children(self.render_preview(cx))
            .children(self.render_settings(cx))
            .children(self.render_about(cx))
            .children(self.render_acl_popup(cx))
            .children(self.render_path_suggestions(cx))
            // Over the preview and the dialogs, because it is started from
            // inside them; under the palette, which is summoned deliberately
            // and should not have a chip sitting on its corner.
            .children(self.render_opening(cx))
            // Last, so it paints over everything. The palette can be summoned
            // from on top of any of these — "Sửa nội dung" only makes sense
            // while a preview is open — and it used to come up *behind* the
            // preview modal, half of it hidden by the thing it acts on.
            .children(self.render_palette(cx))
    }
}

// ------------------------------------------------------------------ elements

/// A section label with an add button. The button was the missing half: every
/// way to create a profile or bucket existed only as a keyboard shortcut or as
/// buttons that appeared when the list was empty and vanished once it was not.
fn section_header(text: &'static str, id: &'static str, theme: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .pt_2()
        .pb_0p5()
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
        .text_color(if selected {
            theme.text
        } else {
            theme.text_muted
        })
        .when(selected, |this| this.bg(theme.selected))
        .hover(|this| this.bg(theme.hover))
        .child(label)
}

fn action_button(id: &'static str, label: &'static str, theme: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(BUTTON_HEIGHT))
        .px_2()
        .flex()
        .items_center()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .bg(theme.hover)
        .text_color(theme.text)
        .hover(|this| this.bg(theme.selected))
        .child(label)
}

fn detail_action_button(
    id: &'static str,
    label: &'static str,
    icon_name: &'static str,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(24.))
        .px_1p5()
        .flex()
        .items_center()
        .gap_1()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .bg(theme.hover)
        .text_color(theme.text)
        .hover(|this| this.bg(theme.selected))
        .child(sized_icon(icon_name, 12., theme.text_muted))
        .child(label)
}

/// A compact toggle showing what it is set to. The label is faint and the value
/// is not, so the eye lands on the part that changes.
fn setting_chip(
    id: &'static str,
    label: &'static str,
    value: String,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(BUTTON_HEIGHT))
        .px_2()
        .flex()
        .items_center()
        .gap_1p5()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .bg(theme.hover)
        .hover(|this| this.bg(theme.selected))
        .child(div().text_color(theme.text_faint).child(label))
        .child(
            div()
                .text_color(theme.text)
                .child(SharedString::from(value)),
        )
}

/// One `label: value` line in the inspector.
fn detail_row(label: &'static str, value: String, theme: Theme) -> impl IntoElement {
    let id = SharedString::from(format!("detail-row-{label}-{}", animation_key_hash(&value)));

    div()
        .flex()
        .gap_2()
        .child(div().w(px(72.)).text_color(theme.text_faint).child(label))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(ellipsis_text(id, value, 38, theme.text)),
        )
}

fn preview_detail_row(label: &'static str, value: String, theme: Theme) -> impl IntoElement {
    let id = SharedString::from(format!(
        "preview-detail-row-{label}-{}",
        animation_key_hash(&value)
    ));

    div()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(div().text_color(theme.text_faint).child(label))
        .child(ellipsis_text(id, value, 34, theme.text))
}

fn preview_detail_section(title: impl Into<SharedString>, theme: Theme) -> gpui::Div {
    let title = title.into();
    div()
        .p_2()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_lg()
        .bg(theme.hover)
        .child(div().text_xs().text_color(theme.text_faint).child(title))
}

fn inspector_section(title: impl Into<SharedString>, theme: Theme) -> gpui::Div {
    let title = title.into();
    div()
        .p_2()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_lg()
        .bg(theme.modal)
        .border_1()
        .border_color(theme.border)
        .child(div().text_xs().text_color(theme.text_faint).child(title))
}

fn preview_shell(theme: Theme) -> gpui::Hsla {
    if theme.text.l > 0.5 {
        gpui::rgba(0x17191dde).into()
    } else {
        gpui::rgba(0xffffffd9).into()
    }
}

fn preview_veil(theme: Theme) -> gpui::Hsla {
    if theme.text.l > 0.5 {
        gpui::rgba(0xffffff0f).into()
    } else {
        gpui::rgba(0xffffff99).into()
    }
}

fn acl_unavailable_copy(support: Option<Support>) -> Option<(&'static str, &'static str)> {
    match support {
        Some(Support::No) => Some((
            "Bucket này đã tắt ACL",
            "Bucket/provider không hỗ trợ chỉnh ACL. Dùng bucket policy, IAM hoặc policy của provider để phân quyền.",
        )),
        Some(Support::Forbidden) => Some((
            "Token không có quyền ACL",
            "Token hiện tại không đọc được ACL. Cấp quyền ACL tương ứng hoặc dùng policy thay thế.",
        )),
        _ => None,
    }
}

fn acl_unavailable_notice(
    summary: &'static str,
    detail: &'static str,
    theme: Theme,
) -> impl IntoElement {
    div()
        .p_2()
        .flex()
        .flex_col()
        .gap_1()
        .rounded_lg()
        .bg(theme.hover)
        .border_1()
        .border_color(theme.border)
        .child(div().text_color(theme.text).child(summary))
        .child(
            div()
                .text_color(theme.text_faint)
                .child(SharedString::from(detail)),
        )
}

fn acl_status_chip(public: bool, theme: Theme) -> impl IntoElement {
    div()
        .px_1p5()
        .py_0p5()
        .rounded_md()
        .bg(if public { theme.danger } else { theme.hover })
        .text_color(if public {
            theme.text_on_accent
        } else {
            theme.text
        })
        .child(if public { "Công khai" } else { "Riêng tư" })
}

fn acl_target_label(selected_objects: usize) -> String {
    match selected_objects {
        0 | 1 => "Áp dụng cho object này".to_string(),
        count => format!("Áp dụng cho {count} tệp đã chọn"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AclPermissionRow {
    grantee: String,
    read: bool,
    write: bool,
    full: bool,
    public: bool,
}

fn acl_permission_rows(acl: &ObjectAcl) -> Vec<AclPermissionRow> {
    let mut seen = HashSet::new();
    let mut grantees = Vec::new();

    for grantee in [&acl.owner, "Mọi người", "Người dùng đã xác thực"] {
        if !grantee.is_empty() && seen.insert(grantee.to_string()) {
            grantees.push(grantee.to_string());
        }
    }

    for grant in &acl.grants {
        if !grant.grantee.is_empty() && seen.insert(grant.grantee.clone()) {
            grantees.push(grant.grantee.clone());
        }
    }

    grantees
        .into_iter()
        .map(|grantee| {
            let grants = acl.grants.iter().filter(|grant| grant.grantee == grantee);
            let mut row = AclPermissionRow {
                grantee: grantee.clone(),
                read: false,
                write: false,
                full: false,
                public: false,
            };
            for grant in grants {
                row.public |= grant.public;
                match grant.permission.as_str() {
                    "FULL_CONTROL" => {
                        row.read = true;
                        row.write = true;
                        row.full = true;
                    }
                    "READ" => row.read = true,
                    "WRITE" => row.write = true,
                    _ => {}
                }
            }
            row
        })
        .collect()
}

fn acl_permission_table(acl: &ObjectAcl, theme: Theme) -> impl IntoElement {
    div()
        .rounded_lg()
        .bg(preview_veil(theme))
        .p_2()
        .flex()
        .flex_col()
        .gap_1()
        .text_xs()
        .child(acl_permission_header(theme))
        .children(
            acl_permission_rows(acl)
                .into_iter()
                .map(|row| acl_permission_row(row, theme)),
        )
}

fn acl_permission_header(theme: Theme) -> impl IntoElement {
    div()
        .h(px(20.))
        .px_2()
        .flex()
        .items_center()
        .gap_1()
        .text_color(theme.text_faint)
        .child(div().flex_1().min_w(px(0.)).child("Ai"))
        .child(acl_permission_heading("Đọc"))
        .child(acl_permission_heading("Ghi"))
        .child(acl_permission_heading("Toàn"))
}

fn acl_permission_heading(label: &'static str) -> impl IntoElement {
    div()
        .w(px(42.))
        .flex_shrink_0()
        .text_align(gpui::TextAlign::Center)
        .child(label)
}

fn acl_permission_row(row: AclPermissionRow, theme: Theme) -> impl IntoElement {
    let grantee = row.grantee.clone();
    let grantee_tooltip = grantee.clone();

    div()
        .min_h(px(26.))
        .px_2()
        .flex()
        .items_center()
        .gap_1()
        .rounded_md()
        .text_color(if row.public {
            theme.danger
        } else {
            theme.text_muted
        })
        .child(
            div()
                .id(SharedString::from(format!(
                    "acl-grantee-{}",
                    animation_key_hash(&grantee)
                )))
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .child(SharedString::from(elide_middle(&grantee, 28)))
                .tooltip(move |window, cx| Tooltip::new(grantee_tooltip.clone()).build(window, cx)),
        )
        .child(acl_permission_cell(row.read, row.public, theme))
        .child(acl_permission_cell(row.write, row.public, theme))
        .child(acl_permission_cell(row.full, row.public, theme))
}

fn acl_permission_cell(checked: bool, public: bool, theme: Theme) -> impl IntoElement {
    div()
        .w(px(42.))
        .flex_shrink_0()
        .flex()
        .justify_center()
        .child(
            div()
                .size(px(12.))
                .rounded_sm()
                .border_1()
                .flex()
                .items_center()
                .justify_center()
                .border_color(if checked {
                    if public {
                        theme.danger
                    } else {
                        theme.accent
                    }
                } else {
                    theme.border_strong
                })
                .when(checked, |this| {
                    this.bg(if public { theme.danger } else { theme.accent })
                })
                .when(checked, |this| {
                    this.child(sized_icon("check", 9., theme.text_on_accent))
                }),
        )
}

/// A duration in the coarsest unit that still says something useful. Seconds
/// past an hour are noise, and "3600 giây" makes the reader do the arithmetic.
fn format_duration(seconds: u64) -> String {
    match seconds {
        s if s < 60 => format!("{s} giây"),
        s if s < 3600 => format!("{} phút", s / 60),
        s if s < 86_400 => format!("{} giờ {} phút", s / 3600, (s % 3600) / 60),
        s => format!("{} ngày", s / 86_400),
    }
}

/// How long ago something was fetched, in one unit.
///
/// Not `format_duration`: that one is a countdown and keeps the minutes past an
/// hour, which reads as precision an age does not have.
///
/// No seconds arm, for the same reason. This value is computed during a repaint
/// and nothing schedules another one, so "12 giây" is only true for the instant
/// it was drawn and then quietly rots. Everything under a minute says "vừa
/// xong", which stays true for as long as anybody is looking.
///
/// "trước" is on every arm, because a bare duration in this app already means
/// time *remaining* — the transfer queue renders "còn 2 phút" three lines from
/// here, and "2 phút" alone would read as a countdown.
///
/// A negative age means the system clock went backwards between the fetch and
/// now; clamped, because "-3 phút" reads as a bug in the app rather than one in
/// the clock.
fn format_age(seconds: i64) -> String {
    match seconds.max(0) {
        s if s < 60 => "vừa xong".to_string(),
        s if s < 3600 => format!("{} phút trước", s / 60),
        s if s < 86_400 => format!("{} giờ trước", s / 3600),
        s => format!("{} ngày trước", s / 86_400),
    }
}

/// Shortens an opaque token so it stays on one line.
///
/// An ETag or key id has no word boundaries, so letting it wrap splits a hex
/// string across two lines and reads as corrupted data rather than as a long
/// value. The middle is dropped because both ends are what people compare.
fn elide_middle(value: &str, max_chars: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max_chars || max_chars < 5 {
        return value.to_string();
    }
    let keep = max_chars - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    format!(
        "{}…{}",
        chars[..head].iter().collect::<String>(),
        chars[chars.len() - tail..].iter().collect::<String>()
    )
}

/// Which build this is, in one line.
///
/// The name travels with the number because this string is written down by
/// hand into bug reports and read back off screenshots, and "0.1.0" alone
/// names no program.
fn version_line() -> String {
    format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

/// What the About dialog prints for a folder it may not know.
///
/// `None` is reachable without anything being broken — the settings store is
/// absent when no config directory resolves — and a blank there would read as
/// a half-drawn row rather than as an answer.
fn dir_label(dir: Option<&Path>, max_chars: usize) -> String {
    match dir {
        Some(dir) => elide_middle(&dir.display().to_string(), max_chars),
        None => "Không rõ".to_string(),
    }
}

/// The nearest folder that exists at or above `dir`.
///
/// The crash folder is created by the first crash, so on a healthy machine the
/// path the dialog names is not there yet. Handing that path to `opener` fails
/// with the system complaining about a missing file, which reads as a broken
/// button; opening the folder the crashes will appear in answers the question
/// instead.
///
/// Infallible: `ancestors()` on an absolute path ends at `/`, which exists, and
/// both callers pass absolute paths from `dirs::config_dir()`. An `Option` here
/// bought a `None` arm and an error message that nothing could ever reach.
fn revealable(dir: &Path) -> &Path {
    dir.ancestors().find(|dir| dir.exists()).unwrap_or(dir)
}

/// One icon, tinted to `color` and sized to the row it sits in.
///
/// 16px is the size the set is drawn for; scaling a 24-grid stroke much beyond
/// that makes the line weight visibly different from its neighbours.
fn icon(name: &'static str, color: gpui::Hsla) -> impl IntoElement {
    sized_icon(name, 16., color)
}

/// One of this app's icons, in the shape the menu component wants.
///
/// `Icon::empty().path(..)` rather than the library's own `IconName`: that enum
/// points at an icon set this app does not ship, so using it would render
/// blanks beside every menu label.
fn menu_icon(name: &'static str) -> gpui_component::Icon {
    gpui_component::Icon::empty().path(SharedString::from(format!("icons/{name}.svg")))
}

/// An icon at a chosen size. 16px suits a button; a chevron sitting inline with
/// 12px label text needs to be smaller or it overpowers the word beside it.
fn sized_icon(name: &'static str, size: f32, color: gpui::Hsla) -> impl IntoElement {
    gpui::svg()
        .path(SharedString::from(format!("icons/{name}.svg")))
        .size(px(size))
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
        .cursor_pointer()
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
        .h(px(BUTTON_HEIGHT))
        .px_2()
        .flex()
        .items_center()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .text_color(theme.text)
        .bg(theme.hover)
        .hover(|style| style.bg(theme.selected))
        .child(label)
}

fn danger_button(id: &'static str, label: SharedString, theme: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(BUTTON_HEIGHT))
        .px_2()
        .flex()
        .items_center()
        .py_1()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .bg(theme.danger)
        .text_color(theme.text_on_accent)
        .child(label)
}

/// Why the sidebar's rows cannot be used, or `None` when they can.
///
/// Keyed on the client, which is what every one of those rows would need: the
/// `+` beside BUCKETS checks it, `open` returns early without it, and so does
/// `refresh_buckets`. An earlier draft keyed on `active_profile` instead and
/// disagreed with all three — a connect that fails after the profile is chosen
/// leaves a profile selected and no client, and every row stayed lit.
///
/// A connect in flight counts as live. It is about to be, and blinking the
/// whole sidebar grey for the second a startup takes is worse than a row that
/// does nothing for that second.
///
/// One sentence, not two: telling the difference between "no profile chosen"
/// and "the chosen one did not connect" is the failure panel's job, and it is
/// already saying it in more words than a tooltip has room for.
fn sidebar_blocked_reason(connected: bool) -> Option<&'static str> {
    if connected {
        None
    } else {
        Some("Chưa kết nối tới profile nào")
    }
}

/// One row of the sidebar: an icon, a label, and a highlight when you are on it.
///
/// **Every** row in there is this shape — the two nav entries at the top, the
/// pinned places, the bucket list, and Cài đặt with Giới thiệu at the foot. It
/// used to be three: nav rows carried an icon, bucket rows carried none — so their text
/// began where everyone else's *icon* did, and the column had a ragged left
/// edge — and the pinned list was a size smaller again on a theory about
/// sub-lists that only ever read as an accident.
///
/// `blocked` is the sentence saying why the row cannot be used, and dims it
/// instead of removing it. A nav entry that comes and goes is one nobody learns
/// the position of, and a window that changes shape the moment the last profile
/// is deleted hides the app's own layout from the person least likely to know
/// it. The selected background stays even when blocked — it reports which page
/// is on screen, which is still true — but nothing else about the row does.
fn sidebar_item(
    id: SharedString,
    icon: &'static str,
    label: SharedString,
    active: bool,
    blocked: Option<&'static str>,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    sidebar_item_with_badge(id, icon, label, None, active, blocked, theme)
}

/// The same row with a count at its trailing edge.
///
/// Split out rather than adding an argument to every call site: five of the six
/// sidebar rows have nothing to count, and `None` repeated five times is noise
/// at the places that read the clearest without it.
fn sidebar_item_with_badge(
    id: SharedString,
    icon: &'static str,
    label: SharedString,
    badge: Option<SharedString>,
    active: bool,
    blocked: Option<&'static str>,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    let row_id = id.clone();
    let label_id = SharedString::from(format!("sidebar-label-{}", animation_key_hash(id.as_ref())));
    let label_color = match (blocked.is_some(), active) {
        (true, _) => theme.text_faint,
        (false, true) => theme.text,
        (false, false) => theme.text_muted,
    };

    div()
        .id(row_id)
        .px_2()
        .py_1()
        .rounded_md()
        .flex()
        .items_center()
        .gap_1p5()
        .text_sm()
        .when(active, |this| this.bg(theme.selected))
        .text_color(label_color)
        // A hover highlight and a hand cursor on a row whose click is dropped
        // are both promises the row cannot keep.
        .when_else(
            blocked.is_some(),
            |this| this.cursor_default(),
            |this| this.cursor_pointer().hover(|this| this.bg(theme.hover)),
        )
        .when_some(blocked, |this, reason| {
            this.tooltip(move |window, cx| Tooltip::new(reason).build(window, cx))
        })
        .child(sized_icon(
            icon,
            13.,
            if active && blocked.is_none() {
                theme.accent
            } else {
                theme.text_faint
            },
        ))
        .child(div().flex_1().min_w(px(0.)).child(ellipsis_text(
            label_id,
            label.to_string(),
            28,
            label_color,
        )))
        .children(badge.map(|badge| {
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(if active { theme.text } else { theme.text_faint })
                .child(badge)
        }))
}

/// The number beside "Hàng đợi" in the sidebar, or nothing when idle.
///
/// One number, not a sentence. The status bar could afford "2 đang chạy, 3 chờ
/// 5 MB/s còn 2 phút"; a 214px row cannot, and the drawer one click away has
/// every one of those facts per job. What the row has to answer is whether
/// there is anything in there at all.
fn queue_badge(stats: &transfer::Stats) -> Option<SharedString> {
    let moving = stats.active + stats.queued;
    if moving > 0 {
        return Some(SharedString::from(moving.to_string()));
    }
    // Failures outlive the transfer that produced them, and they are the one
    // idle state worth a mark: nothing is moving, and something needs looking at.
    if stats.failed > 0 {
        return Some(SharedString::from(format!("{} lỗi", stats.failed)));
    }
    None
}

/// A hairline between two groups on the status bar.
///
/// The bar had none, so profile, region, shortcut hints, the queue button and
/// the version ran together as one undifferentiated strip of faint text at one
/// size. A rule costs a pixel and says which of those belong together.
fn status_divider(theme: Theme) -> impl IntoElement {
    div()
        .w(px(1.))
        .h(px(11.))
        .flex_shrink_0()
        .bg(theme.border_strong)
}

/// A bare glyph you can click, with no button chrome around it.
///
/// For the small controls that live *inside* something else — the × on a tab,
/// the refresh beside a section heading, the × inside a filter field. Distinct
/// from [`icon_button`], which carries a hit target sized for a toolbar.
fn small_icon_button(
    id: SharedString,
    name: &'static str,
    size: f32,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .hover(|this| this.bg(theme.hover))
        .child(sized_icon(name, size, theme.text_faint))
}

fn icon_button(id: &'static str, name: &'static str, theme: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w(px(26.))
        .h(px(HEADER_HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(|this| this.bg(theme.hover))
        .child(icon(name, theme.text_muted))
}

/// Where the highlight goes next in the path box's suggestions.
///
/// `None` is a real position, not "nothing selected": it means *the text you
/// typed*, which is what the box is for. So walking off either end lands there
/// rather than wrapping — someone arrowing back up past the first row is on
/// their way to the text, not to the bottom of the list.
fn next_choice(current: Option<usize>, down: bool, count: usize) -> Option<usize> {
    match (current, down) {
        (None, true) => Some(0),
        // Up from the text goes to the last row, which is the one nearest it.
        (None, false) => Some(count - 1),
        (Some(index), true) if index + 1 < count => Some(index + 1),
        (Some(_), true) => None,
        (Some(0), false) => None,
        (Some(index), false) => Some(index - 1),
    }
}

/// The trailing crumbs to draw, and whether anything was dropped.
///
/// **The end of the path, never the start.** The last segment is where you are
/// standing and the one before it is the usual way back; the middle of a long
/// prefix is the part nobody reads. Drawing all of them inside an
/// `overflow_hidden` row did the opposite of this: at seven levels —
/// `du-an/khach-hang/2026/quy-3/thang-8/hop-dong/ban-nhap/`, nothing exotic —
/// the crumbs pushed the toolbar's own buttons off the right edge, and what got
/// clipped was the tail, which is the one part that had to survive.
///
/// The bucket is not in here; it is drawn separately and always shows, because
/// "which bucket" is the one question a clipped path must still answer.
///
/// Counting rather than measuring is on purpose: pixel-fitting means laying the
/// row out to find out what fits, and the answer would then change as the
/// window resizes under a name. A count is the same every time. It does leave
/// one case standing — three segments each forty characters long still overflow
/// — and `overflow_hidden` remains the backstop for that.
fn visible_crumbs(
    crumbs: Vec<(SharedString, String)>,
    keep: usize,
) -> (bool, Vec<(SharedString, String)>) {
    let keep = keep.max(1);
    if crumbs.len() <= keep {
        return (false, crumbs);
    }
    let dropped = crumbs.len() - keep;
    (true, crumbs.into_iter().skip(dropped).collect())
}

fn crumb(id: SharedString, label: SharedString, theme: Theme) -> gpui::Stateful<gpui::Div> {
    let label_id = SharedString::from(format!("crumb-label-{}", animation_key_hash(id.as_ref())));

    div()
        .id(id)
        .px_1p5()
        .py_0p5()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .text_color(theme.text)
        .hover(|this| this.bg(theme.hover))
        .child(ellipsis_text(label_id, label.to_string(), 22, theme.text))
}

/// How a row is drawn, apart from what it holds. Grouped because the list of
/// them had grown past the point where a call site says anything.
struct RowState {
    position: usize,
    selected: bool,
    cursor: bool,
    thumbnail: Option<std::sync::Arc<gpui::Image>>,
}

fn object_row(
    entry: &Entry,
    state: RowState,
    // Built by the caller because it needs a listener of its own, and a listener
    // needs the view's context. Its click must not also reach the row, or
    // ticking a box would clear the selection and select that one row.
    checkbox: gpui::AnyElement,
    // Same reason as the checkbox.
    actions: gpui::AnyElement,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    let RowState {
        position,
        selected,
        cursor,
        thumbnail,
    } = state;
    let size_label = if entry.is_folder {
        SharedString::from("")
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
        // Every row carries the bar so that turning it on does not shift the
        // text sideways; only the cursor row gets a colour. Selection alone is
        // not enough to show it — ⌘-clicking selects rows the cursor is not on,
        // and the next arrow key moves from the cursor, not from the click.
        .border_l_2()
        .border_color(if cursor {
            theme.accent
        } else {
            gpui::transparent_black()
        })
        .when(selected, |this| this.bg(theme.selected))
        .hover(|this| this.bg(theme.hover))
        // The row number, as Brows3 has it. On a list of twelve hundred files
        // called `file-0001.txt` it is the only thing that says how far down
        // you are.
        .child(
            div()
                .w(px(ROW_NUMBER_WIDTH))
                .flex_shrink_0()
                .text_xs()
                .text_color(theme.text_faint)
                .child(SharedString::from((position + 1).to_string())),
        )
        // The checkbox. The row itself still selects on click, as it always
        // did; this is the second way, for anyone who does not know that
        // ⌘-click and ⇧-click are the first two. Brows3 has it, and so does
        // every web file list.
        .child(checkbox)
        .child(
            div()
                .w(px(22.))
                .flex()
                .items_center()
                // Folders lead, so they carry the accent; files stay quiet.
                // A thumbnail when one has loaded, the type icon otherwise. The
                // slot is the same width either way, so rows do not shift as
                // images arrive.
                .child(match thumbnail {
                    Some(image) => gpui::img(image)
                        .size(px(18.))
                        .rounded_sm()
                        .into_any_element(),
                    None if entry.is_folder => icon("folder", theme.accent).into_any_element(),
                    None => icon("file", theme.text_faint).into_any_element(),
                }),
        )
        .child(
            div()
                .flex_1()
                // A floor, not zero. Letting the pane shrink fixed the
                // inspector being shoved off the window, and then this cell —
                // the one holding the *name* — was the first thing flexbox
                // took the space from, leaving rows that read
                // "6 ☐ 📄 BIN 2.9 MB". Whatever else has to go at a narrow
                // width, it is not the name of the thing.
                .min_w(px(NAME_MIN_WIDTH))
                .flex()
                .items_center()
                .gap_1p5()
                .overflow_hidden()
                .child(ellipsis_text(
                    SharedString::from(format!(
                        "row-name-{position}-{}",
                        animation_key_hash(&entry.key)
                    )),
                    entry.name.clone(),
                    72,
                    theme.text,
                )),
        )
        // Its own column rather than a chip beside the name: a chip sits at
        // whatever x the name happens to end at, so scanning a list for "the
        // CSVs" means reading every row. A column is one glance down.
        .child(
            div()
                .w(px(TYPE_WIDTH))
                .flex_shrink_0()
                .text_xs()
                .text_color(theme.text_faint)
                .children(type_badge(entry)),
        )
        .child(
            div()
                .w(px(84.))
                .flex_shrink_0()
                .text_color(theme.text_muted)
                .child(size_label),
        )
        .child(
            div()
                .w(px(132.))
                .flex_shrink_0()
                .text_color(theme.text_faint)
                .child(SharedString::from(modified)),
        )
        .child(actions)
}

fn object_grid_card(
    entry: &Entry,
    state: RowState,
    checkbox: gpui::AnyElement,
    actions: gpui::AnyElement,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    let RowState {
        position,
        selected,
        cursor,
        thumbnail,
    } = state;
    let badge = type_badge(entry);
    let display_name = if entry.is_folder {
        entry.name.clone()
    } else {
        name_without_type(&entry.name)
    };
    let size_text = (!entry.is_folder).then(|| SharedString::from(format_size(entry.size)));
    let badge_for_meta = badge.clone();
    let has_thumbnail = thumbnail.is_some();
    let media_bg = if entry.is_folder {
        gpui::transparent_black()
    } else if has_thumbnail {
        theme.modal
    } else {
        preview_veil(theme)
    };

    div()
        .id(SharedString::from(format!("grid-card-{position}")))
        .h(px(GRID_ROW_HEIGHT - 12.))
        .flex_1()
        .min_w(px(0.))
        .p_2()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_lg()
        .bg(if selected || cursor {
            theme.selected
        } else {
            gpui::transparent_black()
        })
        .hover(move |this| {
            this.bg(if selected {
                theme.selected
            } else {
                preview_veil(theme)
            })
        })
        .cursor_pointer()
        .child(
            div()
                .h(px(24.))
                .flex()
                .items_center()
                .gap_1()
                .overflow_hidden()
                .child(checkbox)
                .child(div().flex_1())
                .child(actions),
        )
        .child(
            div()
                .h(px(GRID_MEDIA_HEIGHT))
                .w_full()
                .p_1()
                .flex()
                .items_center()
                .justify_center()
                .rounded_lg()
                .bg(media_bg)
                .overflow_hidden()
                .child(
                    div()
                        .h_full()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .overflow_hidden()
                        .bg(if has_thumbnail {
                            preview_veil(theme)
                        } else {
                            gpui::transparent_black()
                        })
                        .child(match thumbnail {
                            Some(image) => div()
                                .h_full()
                                .w_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .overflow_hidden()
                                .child(
                                    gpui::img(image)
                                        .max_w_full()
                                        .max_h(px(GRID_MEDIA_HEIGHT - 12.))
                                        .object_fit(ObjectFit::Contain)
                                        .rounded_md()
                                        .transition_opacity_and_offset(
                                            SharedString::from(format!(
                                                "grid-thumb-in-{position}-{}",
                                                animation_key_hash(&entry.key)
                                            )),
                                            smooth_transition(MOTION_PANEL_MS),
                                            0.0,
                                            1.0,
                                            motion_offset(0.0, 2.0),
                                            motion_offset(0.0, 0.0),
                                        ),
                                )
                                .into_any_element(),
                            None if entry.is_folder => {
                                sized_icon("folder", 46., theme.accent).into_any_element()
                            }
                            None => sized_icon("file", 42., theme.text_faint).into_any_element(),
                        }),
                ),
        )
        .child(
            div()
                .h(px(22.))
                .flex_shrink_0()
                .overflow_hidden()
                .text_sm()
                .child(ellipsis_text(
                    SharedString::from(format!(
                        "grid-name-{position}-{}",
                        animation_key_hash(&entry.key)
                    )),
                    display_name.clone(),
                    28,
                    theme.text,
                )),
        )
        .when_some(size_text, move |this, size| {
            let meta = div()
                .h(px(18.))
                .flex()
                .items_center()
                .gap_1p5()
                .text_xs()
                .text_color(theme.text_faint)
                .when_some(badge_for_meta, |this, badge| {
                    this.child(
                        div()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .bg(theme.hover)
                            .text_color(theme.text_muted)
                            .child(badge),
                    )
                })
                .child(div().child(size));

            this.child(meta)
        })
}

fn grid_row_count(items: usize) -> usize {
    items.div_ceil(GRID_COLUMNS)
}

// ------------------------------------------------------------------- helpers

/// Bandwidth presets the drawer button cycles through, in bytes per second.
/// Zero is unlimited and comes last, so one more click always gets back to it.
const BANDWIDTH_PRESETS: [u64; 4] = [1_000_000, 5_000_000, 20_000_000, 0];

/// Why a listing has no rows to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Emptiness {
    /// The prefix holds nothing. Nothing the user can undo.
    Prefix,
    /// Rows were loaded and the filter is hiding every one of them. Escape
    /// brings them back, so this state comes with a button.
    Filtered,
}

/// Which of the two an empty list is.
///
/// The subtle case is a filter typed against a prefix that was empty to begin
/// with: the filter is not the reason there is nothing, and offering to clear
/// it would send someone chasing a cause that is not there.
fn emptiness(filter: &str, loaded: usize) -> Emptiness {
    if !filter.is_empty() && loaded > 0 {
        Emptiness::Filtered
    } else {
        Emptiness::Prefix
    }
}

/// How many placeholder rows the object list shows while it loads.
///
/// Enough to overflow a tall window, because the container clips what does not
/// fit but cannot invent what is missing: a count that stops short leaves a
/// band of empty floor under the placeholders, which reads as "this folder has
/// twenty things in it" rather than as "still loading".
const SKELETON_ROWS: usize = 48;

/// A placeholder bar, standing in for text that has not arrived.
///
/// Static rather than shimmering. The rows may fade in once, but they do not
/// keep repainting for as long as the wait lasts.
fn skeleton_bar(width: f32, theme: Theme) -> impl IntoElement {
    div().h(px(8.)).w(px(width)).rounded_sm().bg(theme.hover)
}

/// Placeholder rows shaped like the object list.
///
/// Shaped rather than a centred "loading…" so the columns are already where
/// they will be: the real rows arrive into the same geometry instead of shoving
/// a message out of the way.
fn skeleton_rows(theme: Theme) -> impl IntoElement {
    // Varied widths on purpose. Identical bars down the column read as a
    // rendering fault rather than as names of different lengths.
    const WIDTHS: [f32; 7] = [186., 124., 238., 152., 96., 208., 144.];

    div()
        .flex_1()
        .flex()
        .flex_col()
        .overflow_hidden()
        .children((0..SKELETON_ROWS).map(|row| {
            div()
                .h(px(ROW_HEIGHT))
                // Without this, forty-eight rows in a shorter pane each shrink
                // to fit and the placeholders come out half-height — the flex
                // default is to shrink, and the clipping has to be done by the
                // container instead.
                .flex_shrink_0()
                .w_full()
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                // The same slots `object_row` uses, at the same widths, so the
                // real rows land without the columns sliding sideways.
                .child(
                    div()
                        .w(px(ROW_NUMBER_WIDTH))
                        .flex_shrink_0()
                        .child(skeleton_bar(14., theme)),
                )
                .child(
                    div()
                        .w(px(CHECK_WIDTH))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .child(div().size(px(15.)).rounded_sm().bg(theme.hover)),
                )
                .child(
                    div()
                        .w(px(22.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .child(div().size(px(16.)).rounded_sm().bg(theme.hover)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(NAME_MIN_WIDTH))
                        .child(skeleton_bar(WIDTHS[row % WIDTHS.len()], theme)),
                )
                .child(div().w(px(TYPE_WIDTH)).flex_shrink_0())
                .child(div().w(px(84.)).child(skeleton_bar(38., theme)))
                .child(div().w(px(132.)).child(skeleton_bar(94., theme)))
                .child(div().w(px(ACTIONS_WIDTH)).flex_shrink_0())
        }))
        .transition_opacity(
            "listing-skeleton",
            smooth_transition(MOTION_MODAL_MS),
            0.55,
            1.0,
        )
}

/// Placeholder bucket names, for the sidebar while a connection is being made.
fn skeleton_sidebar(theme: Theme) -> impl IntoElement {
    const WIDTHS: [f32; 4] = [108., 82., 130., 96.];

    div().flex().flex_col().gap_1().children(
        WIDTHS
            .iter()
            .map(|&width| {
                div()
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .h(px(ROW_HEIGHT))
                    .child(skeleton_bar(width, theme))
            })
            .collect::<Vec<_>>(),
    )
}

/// Placeholder metadata rows, mirroring `detail_row`'s two columns.
fn skeleton_details(theme: Theme) -> impl IntoElement {
    const WIDTHS: [(f32, f32); 6] = [
        (52., 128.),
        (40., 92.),
        (64., 156.),
        (48., 108.),
        (58., 136.),
        (44., 84.),
    ];

    div()
        .p_3()
        .flex()
        .flex_col()
        .gap_3()
        .children(
            WIDTHS
                .iter()
                .map(|&(label, value)| {
                    div()
                        .flex()
                        .gap_2()
                        .items_center()
                        .child(div().w(px(84.)).child(skeleton_bar(label, theme)))
                        .child(skeleton_bar(value, theme))
                })
                .collect::<Vec<_>>(),
        )
        .transition_opacity(
            "details-skeleton",
            smooth_transition(MOTION_MODAL_MS),
            0.55,
            1.0,
        )
}

/// Which end of the viewport to align a row against when scrolling it into
/// view.
///
/// gpui's non-strict `scroll_to_item` has no "nearest" strategy: once the row
/// is off screen it aligns to whichever end the strategy names, so naming the
/// wrong one turns a one-row step into a full-page jump — arrowing down with
/// `Top` throws the row to the top of the list and everything under it out of
/// sight. The direction of travel picks the end, and then the minimal
/// adjustment and the strategy agree on the same one-row scroll.
fn scroll_edge(down: bool) -> gpui::ScrollStrategy {
    if down {
        gpui::ScrollStrategy::Bottom
    } else {
        gpui::ScrollStrategy::Top
    }
}

/// One command in the palette. Pulled out of the render so the uniform list's
/// processor stays readable.
fn palette_row(
    position: usize,
    command: Command,
    selected: bool,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    let (label, shortcut) = command.label();
    div()
        .id(position)
        .h(px(PALETTE_ROW_HEIGHT))
        .w_full()
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        // Inset and rounded, not an edge-to-edge band: the full-width bar ran
        // into the pane's corner curve on the first row, and a selection that
        // pokes out of its own container reads as a rendering bug.
        .rounded_lg()
        .cursor_pointer()
        .when(selected, |row| row.bg(theme.selected))
        .hover(|row| row.bg(theme.hover))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(theme.text)
                .child(label),
        )
        // The shortcut as a keycap, the way every palette draws one. Bare
        // faint text next to bare faint text is how the old rows managed to
        // hold two facts and show one.
        .when(!shortcut.is_empty(), |row| {
            row.child(
                div()
                    .px_1p5()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme.hover)
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child(shortcut),
            )
        })
}

fn bandwidth_label(limit: u64) -> String {
    if limit == 0 {
        "∞".into()
    } else {
        format!("{} MB/s", limit / 1_000_000)
    }
}

/// The next preset after `current`. An unrecognised limit (set by something
/// other than this button) falls through to the first preset.
fn next_bandwidth_limit(current: u64) -> u64 {
    let at = BANDWIDTH_PRESETS
        .iter()
        .position(|&preset| preset == current);
    match at {
        Some(ix) => BANDWIDTH_PRESETS[(ix + 1) % BANDWIDTH_PRESETS.len()],
        None => BANDWIDTH_PRESETS[0],
    }
}

/// Which heading a line of the transfer queue sits under.
///
/// Four headings rather than one per `JobState`: queued, running and paused all
/// mean the transfer has not happened yet, and a heading each would cut the
/// list into slivers that repeat what the state column beside them already
/// says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueueSection {
    Active,
    Done,
    Failed,
    Canceled,
}

impl QueueSection {
    /// The order the drawer draws headings in.
    const ORDER: [QueueSection; 4] = [
        QueueSection::Active,
        QueueSection::Done,
        QueueSection::Failed,
        QueueSection::Canceled,
    ];

    fn of(state: JobState) -> Self {
        match state {
            JobState::Queued | JobState::Running | JobState::Paused => QueueSection::Active,
            JobState::Done => QueueSection::Done,
            JobState::Failed => QueueSection::Failed,
            JobState::Canceled => QueueSection::Canceled,
        }
    }

    /// Uppercase, like every other section heading in this file — ĐÃ GHIM,
    /// BUCKETS, QUYỀN TRUY CẬP, and the drawer's own "HÀNG ĐỢI {n}" three lines
    /// above. It also stops a heading reading as a state cell: "đang chạy" is
    /// a value the Trạng thái column already prints.
    fn label(self) -> &'static str {
        match self {
            QueueSection::Active => "ĐANG CHẠY",
            QueueSection::Done => "XONG",
            QueueSection::Failed => "HỎNG",
            QueueSection::Canceled => "ĐÃ HUỶ",
        }
    }
}

/// Where a folder row is filed when its files are not all in the same state.
///
/// Anything still moving wins, so a folder does not leave the running section
/// while part of it is still going. After that, "xong" is reserved for a folder
/// that arrived whole: the headings are what someone scans to decide whether
/// anything needs them, and a folder with one broken or cancelled file filed
/// under "xong" hides exactly the file they were looking for.
fn folder_section(members: &[Job]) -> QueueSection {
    for section in [
        QueueSection::Active,
        QueueSection::Failed,
        QueueSection::Canceled,
    ] {
        if members
            .iter()
            .any(|job| QueueSection::of(job.state) == section)
        {
            return section;
        }
    }
    QueueSection::Done
}

/// The folder row a job belongs to, or `None` for a key at the bucket root.
///
/// Grouped on the shared parent prefix, which is not quite what it should be: a
/// `Job` carries an id, a direction, a bucket, a key, a size and a state, and
/// nothing that says "these two were queued by the same dropped folder". So two
/// files uploaded separately into one folder also collapse into a single row.
/// That is the safe direction to be wrong in — the row opens to show exactly
/// which files it stands for — and it is the only one available without a batch
/// id on the job itself.
///
/// It also means a folder with subfolders arrives as one row per level rather
/// than one row for the drop, since each level is its own prefix.
///
/// Direction and bucket are part of the key because an upload and a download of
/// the same prefix are different work; one progress bar over bytes moving in
/// opposite directions would say nothing.
fn folder_group_key(job: &Job) -> Option<String> {
    let prefix = parent_prefix_of(&job.key);
    if prefix.is_empty() {
        return None;
    }
    let direction = match job.direction {
        transfer::Direction::Upload => "upload",
        transfer::Direction::Download => "download",
    };
    // `|` separates the three parts unambiguously: a bucket name cannot contain
    // one, so a key that does cannot be mistaken for another bucket's.
    Some(format!("{direction}|{}|{prefix}", job.bucket))
}

/// The prefix out of a group key, which is `direction|bucket|prefix/`.
///
/// Trailing slash removed: it is how a prefix is written everywhere in the S3
/// API, and nowhere in a name a person reads.
fn folder_name_of_key(group_key: &str) -> String {
    group_key
        .splitn(3, '|')
        .nth(2)
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string()
}

/// What a folder row prints in the name column.
///
/// The prefix on its own when that is enough to tell the rows apart, and the
/// bucket in front of it when it is not. Direction last, for the one case both
/// leave ambiguous: the same prefix in the same bucket going both ways at once.
fn folder_display_name(group_key: &str, plain_counts: &HashMap<String, usize>) -> String {
    let mut parts = group_key.splitn(3, '|');
    let direction = parts.next().unwrap_or_default();
    let bucket = parts.next().unwrap_or_default();
    let prefix = parts.next().unwrap_or_default().trim_end_matches('/');

    if plain_counts.get(prefix).copied().unwrap_or(0) <= 1 {
        return prefix.to_string();
    }
    let direction = match direction {
        "download" => " ↓",
        _ => " ↑",
    };
    format!("{bucket}/{prefix}{direction}")
}

/// What a folder row prints in the Trạng thái column.
///
/// Not the section heading's word. A folder of files that have not started is
/// filed under ĐANG CHẠY — nothing has failed, so it belongs with the work
/// still to do — but printing "đang chạy" beside it while every child row says
/// "chờ" is one column giving two answers in one frame. The order is the one a
/// reader scans: what is happening, then what is waiting, then how it ended.
fn folder_state_label(members: &[Job]) -> &'static str {
    let any = |wanted: JobState| members.iter().any(|job| job.state == wanted);
    if any(JobState::Running) {
        "đang chạy"
    } else if any(JobState::Queued) {
        "chờ"
    } else if any(JobState::Paused) {
        "tạm dừng"
    } else if any(JobState::Failed) {
        "lỗi"
    } else if any(JobState::Canceled) {
        "đã huỷ"
    } else {
        "xong"
    }
}

/// A folder's jobs, collapsed onto one line.
#[derive(Clone, Debug)]
struct FolderRow {
    /// Identity of the group; also what the expanded set holds and what the
    /// row's element ids are built from.
    key: String,
    name: String,
    section: QueueSection,
    expanded: bool,
    count: usize,
    /// What the Trạng thái column prints. Kept apart from `section`, which only
    /// decides which heading the row files under.
    state_label: &'static str,
    transferred: u64,
    size: u64,
    /// Every job on the row, so its buttons act on the whole folder.
    ids: Vec<i64>,
    pausable: bool,
    resumable: bool,
}

impl FolderRow {
    fn build(key: String, name: String, members: &[Job], expanded: bool) -> Self {
        Self {
            name,
            key,
            section: folder_section(members),
            state_label: folder_state_label(members),
            expanded,
            count: members.len(),
            transferred: members.iter().map(|job| job.transferred).sum(),
            size: members.iter().map(|job| job.size).sum(),
            ids: members.iter().map(|job| job.id).collect(),
            pausable: members
                .iter()
                .any(|job| matches!(job.state, JobState::Running | JobState::Queued)),
            resumable: members
                .iter()
                .any(|job| matches!(job.state, JobState::Paused | JobState::Failed)),
        }
    }

    /// The same rule one job uses: with nothing to divide by, a finished folder
    /// is full and anything else is empty, rather than a NaN-wide bar.
    fn fraction(&self) -> f32 {
        if self.size == 0 {
            return if self.section == QueueSection::Done {
                1.0
            } else {
                0.0
            };
        }
        (self.transferred as f32 / self.size as f32).clamp(0.0, 1.0)
    }
}

/// One line of the transfer drawer.
#[derive(Clone, Debug)]
enum QueueRow {
    Heading(QueueSection),
    Folder(FolderRow),
    /// `child` marks a file drawn under an open folder row.
    Job {
        job: Job,
        child: bool,
    },
}

/// Lays the queue out the way the drawer draws it: a heading over each
/// non-empty status section, the files of one folder collapsed onto a single
/// row, and that folder's files under it when it is open.
fn queue_rows(jobs: &[Job], expanded: &HashSet<String>) -> Vec<QueueRow> {
    /// A folder row and its files, or a file standing alone.
    enum Item {
        Folder(FolderRow, Vec<Job>),
        Job(Job),
    }

    let mut first_seen: HashMap<String, usize> = HashMap::new();
    let mut members: HashMap<String, Vec<Job>> = HashMap::new();
    for (ix, job) in jobs.iter().enumerate() {
        let Some(key) = folder_group_key(job) else {
            continue;
        };
        first_seen.entry(key.clone()).or_insert(ix);
        members.entry(key).or_default().push(job.clone());
    }
    // One file under a prefix is not a folder transfer; hiding it behind a row
    // that has to be opened to see the single thing inside is worse than the
    // flat row it replaces.
    members.retain(|_, files| files.len() > 1);

    // Keyed by where the entry first appears in the queue, so folders and lone
    // files stay in the order they were queued instead of in whatever order the
    // grouping happened to visit them.
    // Two folders can share a prefix and still be different work — the same
    // `photos/` in two buckets, or one being uploaded while the other comes
    // down. Left alone they draw as two rows with the same name, the same
    // count and the same bar, and nothing on the row says which is which.
    // Qualified only where that actually happens, so the ordinary row stays
    // short.
    let mut plain: HashMap<String, usize> = HashMap::new();
    for key in members.keys() {
        *plain.entry(folder_name_of_key(key)).or_default() += 1;
    }

    let mut items: Vec<(usize, Item)> = members
        .iter()
        .map(|(key, files)| {
            let name = folder_display_name(key, &plain);
            let row = FolderRow::build(key.clone(), name, files, expanded.contains(key));
            (first_seen[key], Item::Folder(row, files.clone()))
        })
        .collect();
    for (ix, job) in jobs.iter().enumerate() {
        let grouped = folder_group_key(job).is_some_and(|key| members.contains_key(&key));
        if !grouped {
            items.push((ix, Item::Job(job.clone())));
        }
    }
    items.sort_by_key(|(ix, _)| *ix);

    let mut rows = Vec::new();
    for section in QueueSection::ORDER {
        let mut body = Vec::new();
        for (_, item) in &items {
            match item {
                Item::Folder(row, files) if row.section == section => {
                    body.push(QueueRow::Folder(row.clone()));
                    if row.expanded {
                        body.extend(files.iter().map(|job| QueueRow::Job {
                            job: job.clone(),
                            child: true,
                        }));
                    }
                }
                Item::Job(job) if QueueSection::of(job.state) == section => {
                    body.push(QueueRow::Job {
                        job: job.clone(),
                        child: false,
                    });
                }
                _ => {}
            }
        }
        if !body.is_empty() {
            rows.push(QueueRow::Heading(section));
            rows.append(&mut body);
        }
    }
    rows
}

/// Expiry choices for a presigned URL. Capped per-credential-type at sign time,
/// so a temporary-credential profile silently gets the shorter of the two.
const PRESIGN_PRESETS: [(&str, Duration); 4] = [
    ("15 phút", Duration::from_secs(900)),
    ("1 giờ", Duration::from_secs(3600)),
    ("24 giờ", Duration::from_secs(24 * 3600)),
    ("7 ngày", Duration::from_secs(7 * 24 * 3600)),
];

/// Turns fetched bytes into something renderable. Text that is not valid UTF-8
/// is treated as unsupported rather than shown as replacement characters —
/// mojibake looks like corruption of the object itself.
/// How much of a delimited file the preview keeps. A preview is for deciding
/// whether this is the right file, not for reading it.
const TABLE_ROWS: usize = 200;
const TABLE_COLUMNS: usize = 24;
/// Where a cell gets elided. Wide enough for a date, a name or an id; a column
/// of essays would otherwise push every other column off the panel.
const TABLE_CELL_CHARS: usize = 28;

/// Splits delimited text into rows, following RFC 4180.
///
/// Hand-rolled rather than pulled in: the rules are four lines long and the
/// naive `split(',')` that everyone writes instead is wrong on the first file
/// with a comma inside a quoted field — which, for a CSV, is most of them.
fn parse_rows(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                // Doubled quotes are one literal quote; a single one ends the
                // quoting.
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && field.is_empty() {
            quoted = true;
        } else if c == delimiter {
            row.push(std::mem::take(&mut field));
        } else if c == '\n' {
            row.push(std::mem::take(&mut field));
            rows.push(std::mem::take(&mut row));
        } else if c != '\r' {
            // A bare `\r` is the other half of a CRLF; the `\n` ends the row.
            field.push(c);
        }
    }

    // A file that does not end in a newline still has a last row. One that does
    // must not gain an empty one.
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

fn parse_table(text: &str, delimiter: char) -> Table {
    let mut rows = parse_rows(text, delimiter);
    if rows.is_empty() {
        return Table {
            headers: Vec::new(),
            rows: Vec::new(),
            hidden_rows: 0,
            hidden_columns: 0,
        };
    }

    let widest = rows.iter().map(Vec::len).max().unwrap_or(0);
    let hidden_columns = widest.saturating_sub(TABLE_COLUMNS);
    for row in &mut rows {
        row.truncate(TABLE_COLUMNS);
        // Short rows are padded so the columns stay aligned. A ragged file is
        // common and is not a reason to refuse to show it.
        row.resize(widest.min(TABLE_COLUMNS), String::new());
    }

    let headers = rows.remove(0);
    let hidden_rows = rows.len().saturating_sub(TABLE_ROWS);
    rows.truncate(TABLE_ROWS);

    Table {
        headers,
        rows,
        hidden_rows,
        hidden_columns,
    }
}

/// A parsed table, laid out in columns.
fn render_table(table: &Table, mono: SharedString, theme: Theme) -> impl IntoElement {
    let widths = table_column_widths(table);
    let cell = move |text: &String, width: f32, color: gpui::Hsla| {
        div()
            .w(px(width))
            .flex_shrink_0()
            .pr_2()
            .overflow_hidden()
            // Without this a cell whose text is a few pixels wider than its
            // computed box wraps onto a second line and every column after it
            // stops lining up. The width comes from a character count, and a
            // character count is never exactly a pixel count.
            .whitespace_nowrap()
            .text_color(color)
            .child(SharedString::from(elide_middle(text, TABLE_CELL_CHARS)))
    };

    div()
        .font_family(mono)
        .text_xs()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .pb_1()
                .mb_1()
                .border_b_1()
                .border_color(theme.border)
                .children(
                    table
                        .headers
                        .iter()
                        .enumerate()
                        .map(|(ix, text)| cell(text, widths[ix], theme.text)),
                ),
        )
        .children(table.rows.iter().map(|row| {
            div().flex().children(
                row.iter()
                    .enumerate()
                    .map(|(ix, text)| cell(text, widths[ix], theme.text_muted)),
            )
        }))
        // Said out loud, because a preview that silently stops looks like a file
        // that ends there.
        .when(table.hidden_rows > 0 || table.hidden_columns > 0, |this| {
            this.child(
                div()
                    .pt_1()
                    .text_color(theme.text_faint)
                    .child(SharedString::from(hidden_note(table))),
            )
        })
}

/// Column widths in pixels, from the longest cell in each column.
///
/// Equal-width columns would waste the panel on a table of short ids and elide
/// the one column that has anything in it.
fn table_column_widths(table: &Table) -> Vec<f32> {
    // Menlo at this size, measured a little generously. Approximate on purpose:
    // a monospace face makes it close enough for the columns to line up, and
    // rounding up costs a few pixels while rounding down clips text.
    const CHAR_WIDTH: f32 = 7.6;
    const PADDING: f32 = 12.0;

    let columns = table.headers.len();
    (0..columns)
        .map(|ix| {
            let longest = std::iter::once(&table.headers[ix])
                .chain(table.rows.iter().filter_map(|row| row.get(ix)))
                .map(|text| text.chars().count())
                .max()
                .unwrap_or(0)
                .clamp(3, TABLE_CELL_CHARS);
            longest as f32 * CHAR_WIDTH + PADDING
        })
        .collect()
}

/// What a truncated preview leaves out, in words.
fn hidden_note(table: &Table) -> String {
    match (table.hidden_rows, table.hidden_columns) {
        (0, 0) => String::new(),
        (rows, 0) => format!("còn {rows} dòng nữa"),
        (0, columns) => format!("còn {columns} cột nữa"),
        (rows, columns) => format!("còn {rows} dòng và {columns} cột nữa"),
    }
}

fn build_preview(kind: PreviewKind, key: &str, bytes: Vec<u8>) -> Preview {
    match kind {
        PreviewKind::Image => match image_format_for_key(key) {
            Some(format) => {
                Preview::Image(std::sync::Arc::new(gpui::Image::from_bytes(format, bytes)))
            }
            None => Preview::Handoff("Không nhận ra định dạng ảnh".into()),
        },
        PreviewKind::Text => match String::from_utf8(bytes) {
            Ok(text) => Preview::Text(text.into()),
            Err(_) => Preview::Handoff("Tệp không phải văn bản UTF-8".into()),
        },
        PreviewKind::Table(delimiter) => match String::from_utf8(bytes) {
            Ok(text) => Preview::Table(parse_table(&text, delimiter)),
            Err(_) => Preview::Handoff("Tệp không phải văn bản UTF-8".into()),
        },
        // Never reached with bytes in hand: nothing is fetched for these.
        PreviewKind::None => Preview::Handoff("Không xem trước được kiểu này".into()),
    }
}

/// Objects above this are listed with an icon instead of a thumbnail. Fetching
/// a few visible megabytes is worth it for an image grid; fetching original
/// camera files just to draw a tile is not.
const THUMBNAIL_LIMIT: i64 = 2 * 1024 * 1024;

/// What a preview should try to render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewKind {
    Image,
    Text,
    /// Comma- or tab-delimited text. Worth its own kind because a CSV shown as
    /// raw text is readable and not usable — the whole point of the file is the
    /// columns, and they only line up by accident.
    Table(char),
    /// Everything else.
    ///
    /// Images and text are the whole of what this previews, deliberately.
    /// Drawing a PDF or video means bundling native playback/rasterising
    /// surfaces for one panel. Those files already have a viewer on the
    /// machine, so `None` is not a dead end here: it is the handoff.
    None,
}

/// A delimited file, parsed into rows for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    /// The first row. CSV has no way to declare whether it has a header, so
    /// every tool guesses, and guessing yes is right far more often than not.
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Rows that exist in the file beyond the ones kept. Reported rather than
    /// dropped in silence: a preview that stops at two hundred rows and says
    /// nothing looks like a file with two hundred rows in it.
    pub hidden_rows: usize,
    /// Columns beyond those kept, for the same reason.
    pub hidden_columns: usize,
}

/// Decides from the content type first and the extension second. The type is
/// what the object claims to be; the extension is a fallback for the many
/// objects uploaded as `application/octet-stream`. HTML is the exception:
/// it runs scripts and pulls subresources, so hand it to the system browser.
fn preview_kind(key: &str, content_type: Option<&str>) -> PreviewKind {
    if html_extension(key) {
        return PreviewKind::None;
    }
    if let Some(mime) = content_type {
        let mime = mime.split(';').next().unwrap_or(mime).trim();
        if html_mime(mime) {
            return PreviewKind::None;
        }
        if image_format_for_mime(mime).is_some() {
            return PreviewKind::Image;
        }
        if mime.starts_with("video/") {
            return PreviewKind::None;
        }
        if mime == "text/csv" || mime == "text/tab-separated-values" {
            return PreviewKind::Table(if mime.ends_with("csv") { ',' } else { '\t' });
        }
        // Structured text is still text: JSON and XML are worth reading inline.
        if mime.starts_with("text/")
            || matches!(
                mime,
                "application/json" | "application/xml" | "application/yaml"
            )
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
        "mp4" | "m4v" | "mov" | "webm" | "avi" | "mkv" => PreviewKind::None,
        "csv" => PreviewKind::Table(','),
        "tsv" => PreviewKind::Table('\t'),
        "txt" | "md" | "json" | "xml" | "yaml" | "yml" | "toml" | "log" | "rs" | "py" | "js"
        | "ts" | "css" | "sh" | "sql" => PreviewKind::Text,
        _ => PreviewKind::None,
    }
}

fn html_opens_externally(key: &str, content_type: Option<&str>) -> bool {
    html_extension(key)
        || content_type
            .and_then(|mime| mime.split(';').next())
            .map(str::trim)
            .is_some_and(html_mime)
}

fn html_extension(key: &str) -> bool {
    matches!(
        key.rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "html" | "htm"
    )
}

fn html_mime(mime: &str) -> bool {
    mime.eq_ignore_ascii_case("text/html") || mime.eq_ignore_ascii_case("application/xhtml+xml")
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
    match key
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
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
/// What a scan has found and what it cost, in one line.
///
/// Pure so the wording can be pinned. The distinction that matters is between
/// a finished scan and a stopped one: "12 kết quả" and "12 kết quả (đã dừng)"
/// are different claims about the bucket, and reporting the second as the first
/// tells someone a file is not there when the scan simply never reached it.
struct SearchProgress {
    found: usize,
    scanned: usize,
    requests: usize,
    complete: bool,
    capped: bool,
    running: bool,
}

fn search_summary(progress: SearchProgress) -> String {
    let SearchProgress {
        found,
        scanned,
        requests,
        complete,
        capped,
        running,
    } = progress;
    // A cap is checked before completion: a scan that hit the ceiling on its
    // last page did not cover the bucket, whatever the continuation token says.
    let state = match (capped, complete, running) {
        (true, _, _) => "chạm trần",
        (_, true, _) => "xong",
        (_, false, true) => "đang quét",
        (_, false, false) => "đã dừng",
    };
    format!("{found} kết quả, quét {scanned} mục trong {requests} yêu cầu ({state})")
}

/// What a deep search will spend before it gives up on its own.
///
/// The person who presses Enter is not the person who reads the bill, and a
/// bucket with ten million keys is ten thousand LIST requests. Same numbers
/// Brows3 uses, for the same reason.
const SEARCH_MAX_REQUESTS: usize = 100;
const SEARCH_MAX_KEYS: usize = 100_000;
const SEARCH_MAX_MATCHES: usize = 10_000;

/// Collapses a multi-line error into one paragraph, for display only.
///
/// `anyhow`'s `Debug` output is already broken into indented `Caused by:`
/// lines. Re-wrapping that at a column produces a line of prose followed by a
/// stub of three words, over and over, which looks like a rendering fault. What
/// gets copied to the clipboard stays exactly as the provider wrote it.
/// How wide the label column of a form has to be.
///
/// Fixed at 84 it was fine until a form arrived with `Content-Disposition` in
/// it, which wrapped onto two lines and pushed its own input out of line with
/// every other row. Measured from the longest label instead, so the next long
/// one costs nothing.
fn form_label_width(kind: &FormKind) -> f32 {
    // Inter at this size, rounded up: a label that wraps is worse than a column
    // a few pixels wider than it needed to be.
    const CHAR_WIDTH: f32 = 6.6;
    let longest = kind
        .fields()
        .iter()
        .map(|(label, _, _)| label.chars().count())
        .max()
        .unwrap_or(0);
    (longest as f32 * CHAR_WIDTH + 8.0).max(84.)
}

/// A location typed or pasted as a path.
#[derive(Debug, PartialEq, Eq)]
pub struct S3Path {
    pub bucket: String,
    pub prefix: String,
    /// The `bucket@region` form some tools print. Carried rather than dropped:
    /// the endpoint comes from the profile, so a path naming a different region
    /// would silently go somewhere else, and the caller has to be able to say so.
    pub region: Option<String>,
}

/// Reads `s3://bucket/prefix/`, or the same thing without the scheme.
///
/// This is how a bucket gets reached when the token cannot list buckets — the
/// normal setup on R2 — so it takes the forms people actually have in their
/// clipboard rather than one canonical spelling.
fn parse_s3_path(text: &str) -> Option<S3Path> {
    let text = text.trim();
    let text = text.strip_prefix("s3://").unwrap_or(text);
    // Leading slashes are not trimmed on purpose: `s3:///photos/` names an
    // empty bucket, and trimming would silently promote the first path segment
    // into the bucket and open something the person did not type.

    let (bucket, prefix) = match text.split_once('/') {
        Some((bucket, prefix)) => (bucket, prefix),
        None => (text, ""),
    };
    if bucket.is_empty() {
        return None;
    }

    let (bucket, region) = match bucket.split_once('@') {
        Some((bucket, region)) if !bucket.is_empty() && !region.is_empty() => {
            (bucket, Some(region.to_string()))
        }
        // An `@` with nothing on one side is a typo, not a region.
        Some(_) => return None,
        None => (bucket, None),
    };

    // A prefix listing needs the trailing slash or the delimiter returns
    // nothing; typing it is not something anyone should have to remember.
    let prefix = match prefix.trim_end_matches('/') {
        "" => String::new(),
        trimmed => format!("{trimmed}/"),
    };

    Some(S3Path {
        bucket: bucket.to_string(),
        prefix,
        region,
    })
}

/// Whether a sort can be answered by the order S3 already returns.
///
/// `ListObjectsV2` gives lexicographic order and nothing else, so name-ascending
/// is free and every other sort is a claim about keys not yet fetched.
fn needs_complete_listing(sort: Sort) -> bool {
    !(sort.key == SortKey::Name && sort.ascending)
}

/// Caps on completing a listing. A bucket with a million keys is a thousand
/// requests, and nobody agreed to that by clicking a column header.
const COMPLETE_MAX_REQUESTS: usize = 100;
const COMPLETE_MAX_KEYS: usize = 100_000;

/// The tick itself, so the header box and the row boxes cannot drift apart.
/// A small icon button for the actions column. Quieter than the toolbar's: one
/// of these per row at full weight would be a wall of icons down the side of
/// the list.
fn row_action(
    id: impl Into<gpui::ElementId>,
    name: &'static str,
    label: &'static str,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .hover(|this| this.bg(theme.selected))
        .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
        .child(sized_icon(name, 14., theme.text_muted))
}

fn object_context_menu(
    menu: PopupMenu,
    single: bool,
    can_paste: bool,
    is_folder: bool,
    acl_supported: bool,
) -> PopupMenu {
    // Items the row cannot do are left out rather than shown greyed: a menu of
    // mostly-dead entries is harder to read than a short live one.
    let menu = menu
        .menu_with_icon("Chép", menu_icon("copy"), Box::new(ActionCopy))
        .menu_with_icon("Cắt", menu_icon("cut"), Box::new(ActionCut))
        .menu_with_icon_and_disabled("Dán", menu_icon("paste"), Box::new(ActionPaste), !can_paste)
        .separator();

    let menu = if single {
        menu.menu_with_icon("Đổi tên", menu_icon("rename"), Box::new(ActionRename))
            .menu_with_icon_and_disabled(
                "Nhân bản",
                menu_icon("duplicate"),
                Box::new(ActionDuplicate),
                is_folder,
            )
    } else {
        menu
    };

    // A folder has no metadata of its own and cannot be shared as a link, so
    // those never appear on one.
    let menu = if is_folder {
        let menu = menu.menu_with_icon(
            "Mở trong tab mới",
            menu_icon("external"),
            Box::new(ActionOpenInTab),
        );
        // Only where the provider has ACLs at all. On a bucket with them
        // switched off — the default since 2023 — these two can only ever fail.
        if acl_supported {
            menu.menu_with_icon(
                "Đặt riêng tư cho cả thư mục",
                menu_icon("eye"),
                Box::new(ActionAclPrivate),
            )
            .menu_with_icon(
                "Đặt ai cũng đọc được cho cả thư mục",
                menu_icon("eye"),
                Box::new(ActionAclPublic),
            )
        } else {
            menu
        }
    } else {
        menu.menu_with_icon("Xem trước", menu_icon("eye"), Box::new(ActionPreview))
            .menu_with_icon(
                "Mở bằng app",
                menu_icon("external"),
                Box::new(ActionOpenExternally),
            )
            .menu_with_icon("Chia sẻ", menu_icon("link"), Box::new(ActionShare))
            .menu_with_icon("Chi tiết", menu_icon("info"), Box::new(ActionInspect))
            .menu_with_icon(
                "Sửa header",
                menu_icon("rename"),
                Box::new(ActionEditHeaders),
            )
    };

    menu.menu_with_icon("Tải xuống", menu_icon("download"), Box::new(ActionDownload))
        .separator()
        // Their own group, away from "Chép": that one puts objects on the
        // clipboard for a paste, these two put text on it for somewhere else
        // entirely.
        .menu_with_icon(
            "Chép đường dẫn s3://",
            menu_icon("path"),
            Box::new(ActionCopyPath),
        )
        .menu_with_icon("Chép key", menu_icon("copy"), Box::new(ActionCopyKey))
        .separator()
        .menu_with_icon("Xoá", menu_icon("trash"), Box::new(ActionDelete))
}

/// The palette to paint: the setting when it names one, the system otherwise.
fn theme_for(choice: ThemeChoice, appearance: gpui::WindowAppearance, chrome: Chrome) -> Theme {
    match theme_mode(choice, appearance) {
        crate::theme::Mode::Light => Theme::new(crate::theme::Mode::Light, chrome),
        crate::theme::Mode::Dark => Theme::new(crate::theme::Mode::Dark, chrome),
    }
}

fn theme_mode(choice: ThemeChoice, appearance: gpui::WindowAppearance) -> crate::theme::Mode {
    match choice {
        ThemeChoice::System => crate::theme::Mode::from_appearance(appearance),
        ThemeChoice::Light => crate::theme::Mode::Light,
        ThemeChoice::Dark => crate::theme::Mode::Dark,
    }
}

/// Puts the component library on the same side of light and dark as the app.
///
/// It keeps its own palette for the widgets it draws — inputs, menus, the
/// select — and nothing was ever telling it which one to use. So the text
/// fields stayed dark while everything around them turned light: a black box
/// with white text in the middle of a white toolbar.
fn sync_component_theme(mode: crate::theme::Mode, window: &mut Window, cx: &mut App) {
    let mode = match mode {
        crate::theme::Mode::Light => gpui_component::ThemeMode::Light,
        crate::theme::Mode::Dark => gpui_component::ThemeMode::Dark,
    };
    if gpui_component::Theme::global(cx).mode != mode {
        gpui_component::Theme::change(mode, Some(window), cx);
    }
}

/// One row of the settings panel: what it is, why it matters, and the control.
/// A heading over a group of settings.
///
/// The panel was five rows in a flat stack — theme, preview, bandwidth, threads,
/// a folder path — with nothing saying that the middle three are about moving
/// bytes and the last is about this machine. Five rows is where a flat list
/// stops being a list and starts being a pile.
fn settings_group(title: &'static str, theme: Theme) -> impl IntoElement {
    div()
        .pt_2()
        .flex()
        .items_center()
        .gap_2()
        .child(div().text_xs().text_color(theme.text_faint).child(title))
        .child(div().flex_1().h(px(1.)).bg(theme.border))
}

fn setting_row(
    label: &'static str,
    note: Option<&'static str>,
    control: impl IntoElement,
    theme: Theme,
) -> impl IntoElement {
    div()
        .flex()
        .items_start()
        .gap_3()
        .child(
            // Wider than it was. At 150 every note wrapped onto two or three
            // lines and each row grew to the height of its explanation, so six
            // settings filled a thousand pixels.
            div()
                .w(px(SETTING_LABEL_WIDTH))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .child(div().text_xs().text_color(theme.text).child(label))
                .children(
                    note.map(|note| div().text_xs().text_color(theme.text_faint).child(note)),
                ),
        )
        // Content-sized, and starting at one x for every row. A `flex_1` child
        // stretched every button to the full width of the column, which made
        // "Xoá và tải lại" and a folder path read as text fields rather than as
        // things to press. Left-aligned rather than right, so the controls form
        // a column the eye can run down — the same way macOS lays out its own
        // settings, and the reason the label column has a fixed width at all.
        .child(div().flex_1().min_w(px(0.)).flex().child(control))
}

/// One option among a few. A row of these rather than a dropdown: with three or
/// four choices the row is shorter than the menu that would hide it.
fn choice_chip(
    id: SharedString,
    label: SharedString,
    selected: bool,
    theme: Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(BUTTON_HEIGHT))
        .px_2()
        .flex()
        .items_center()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .bg(if selected {
            theme.selected
        } else {
            theme.hover
        })
        .text_color(if selected {
            theme.text
        } else {
            theme.text_muted
        })
        .hover(|this| this.bg(theme.selected))
        .child(label)
}

/// Whether a preview of an object that size holds all of it.
///
/// The gate on editing. A preview fetches at most the limit, so anything larger
/// arrives cut short — and saving a cut-short copy back over the object deletes
/// everything past the cut, silently, with no way back on a bucket that does not
/// keep versions.
fn preview_holds_whole_object(size: i64, limit: u64) -> bool {
    size >= 0 && (size as u64) <= limit
}

/// What lands on the clipboard when a selection is copied as text.
///
/// Newline-separated because the two things anyone does with several keys —
/// a shell loop and a spreadsheet column — both want one per line, and one long
/// comma run wants neither.
fn location_text(bucket: &str, keys: &[String], as_path: bool) -> String {
    keys.iter()
        .map(|key| {
            if as_path {
                format!("s3://{bucket}/{key}")
            } else {
                key.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A heading with no control beside it, unlike `section_header`.
/// A heading over a group of sidebar rows.
///
/// Shares its padding with [`section_header`], which is the same thing with a
/// button on the end: two headings a few pixels apart vertically is the kind of
/// difference nobody can name and everybody can see.
fn section_label(text: &'static str, theme: Theme) -> impl IntoElement {
    div()
        .px_2()
        .pt_2()
        .pb_0p5()
        .text_xs()
        .text_color(theme.text_faint)
        .child(text)
}

fn check_box(checked: bool, theme: Theme) -> impl IntoElement {
    div()
        .size(px(14.))
        .flex_shrink_0()
        .rounded_sm()
        .border_1()
        .flex()
        .items_center()
        .justify_center()
        .border_color(if checked {
            theme.accent
        } else {
            theme.border_strong
        })
        .when(checked, |this| this.bg(theme.accent))
        .when(checked, |this| {
            this.child(sized_icon("check", 10., theme.text_on_accent))
        })
}

/// The file-type tile shown in the preview and detail headers.
///
/// One helper for all three places that had it, because they were three copies
/// of the same eight lines and had already drifted apart: the detail panel set
/// `text_xs`, the preview header set nothing, and the preview's placeholder set
/// `text_xs` inside a box two and a half times the size.
///
/// A square holding the extension rather than a picture per file family: this
/// app draws its own icon set, and there is no glyph for `webp` that tells
/// anyone more than the word `WEBP` does.
fn kind_tile(label: Option<SharedString>, size: f32, theme: Theme) -> gpui::Div {
    // Everything inside scales with the box, so one tile serves both the badge
    // beside a file name and the placeholder filling the middle of a preview
    // without the larger one being a second implementation that drifts.
    let scale = size / KIND_TILE;
    let tile = div()
        .size(px(size))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .when_else(
            size > KIND_TILE * 1.5,
            |this| this.rounded_xl(),
            |this| this.rounded_md(),
        )
        .bg(theme.hover)
        // A hairline, so it reads as an object sitting on the panel rather than
        // as a hole cut into it. `hover` is a 6% wash; on a light theme it has
        // no edge of its own at all, which is what made the old tile look like
        // a gap the text happened to be standing in.
        .border_1()
        .border_color(theme.border);

    match label {
        Some(label) => tile
            .text_size(px(kind_label_size(label.chars().count()) * scale))
            .text_color(theme.text_muted)
            .child(label),
        // No extension is not a failed lookup, so it does not get a word. A
        // plain sheet says "a file, type unknown" without claiming a type the
        // name never carried. The fallback used to be the Vietnamese "TỆP",
        // whose stacked diacritic was the tallest thing in the box.
        None => tile.child(sized_icon("file", 13. * scale, theme.text_faint)),
    }
}

/// The same slot when the panel is describing a selection rather than one file.
///
/// A circle, deliberately not the square above: a bare number inside a
/// file-type tile reads as a file type called "3".
fn count_tile(count: usize, theme: Theme) -> gpui::Div {
    let label = count.to_string();
    div()
        .size(px(KIND_TILE))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(theme.selected)
        .text_size(px(kind_label_size(label.chars().count())))
        .text_color(theme.accent)
        .child(SharedString::from(label))
}

/// How large the letters inside a [`kind_tile`] can be and still fit it.
///
/// Stepped rather than truncated. `SQLITE` rendered as `SQLI` looks like a
/// drawing fault rather than an abbreviation, and the whole extension is an
/// inch away at the end of the file name regardless — so the one thing this
/// must not do is invent a shorter type than the file has.
fn kind_label_size(chars: usize) -> f32 {
    match chars {
        0..=2 => 11.,
        3 => 10.,
        4 => 8.5,
        5 => 7.5,
        // `type_badge_of` refuses anything past six, so this is the floor.
        _ => 7.,
    }
}

/// The extension, as a chip beside the name.
///
/// `None` for folders and for names with nothing to show: an empty chip is a
/// smudge, and a chip that says `TXT` on `README` would be inventing.
fn type_badge(entry: &Entry) -> Option<SharedString> {
    if entry.is_folder {
        return None;
    }
    type_badge_of(&entry.name)
}

/// The same, for a name with no listing row behind it — the preview knows what
/// it opened but not which `Entry` it came from.
fn type_badge_of(name: &str) -> Option<SharedString> {
    let (_, extension) = name.rsplit_once('.')?;
    // Long enough to be a suffix rather than an extension — `archive.backup2026`
    // is not a file type, and neither is the half of a name after a stray dot.
    if extension.is_empty() || extension.len() > 6 || !extension.chars().all(char::is_alphanumeric)
    {
        return None;
    }
    Some(extension.to_uppercase().into())
}

fn name_without_type(name: &str) -> String {
    let Some((stem, _)) = name.rsplit_once('.') else {
        return name.to_string();
    };

    if stem.is_empty() || type_badge_of(name).is_none() {
        name.to_string()
    } else {
        stem.to_string()
    }
}

/// Which tab a number key selects.
///
/// `9` means the last one rather than the ninth: with three tabs open, ⌘9 has to
/// go somewhere, and every browser sends it to the end.
fn tab_shortcut(digit: &str, open: usize) -> Option<usize> {
    let n: usize = digit.parse().ok()?;
    if n == 0 || open == 0 {
        return None;
    }
    if n == 9 {
        return Some(open - 1);
    }
    (n - 1 < open).then_some(n - 1)
}

/// An empty field means "send no header", which is how a header gets removed.
fn some_if_filled(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FailureCard {
    summary: SharedString,
    detail: String,
    fix: Option<Fix>,
    at: i64,
    count: usize,
}

fn failure_cards(failures: &[Failure]) -> Vec<FailureCard> {
    let mut cards: Vec<FailureCard> = Vec::new();

    for failure in failures.iter().rev() {
        let detail = failure.detail.to_string();
        if let Some(card) = cards.iter_mut().find(|card| {
            card.summary.as_ref() == failure.summary.as_ref()
                && card.detail == detail
                && card.fix == failure.fix
        }) {
            card.count += 1;
            continue;
        }

        cards.push(FailureCard {
            summary: failure.summary.clone(),
            detail,
            fix: failure.fix,
            at: failure.at,
            count: 1,
        });
    }

    cards
}

fn ellipsis_text(
    id: impl Into<gpui::ElementId>,
    text: impl Into<String>,
    max_chars: usize,
    color: gpui::Hsla,
) -> gpui::Stateful<gpui::Div> {
    let full = text.into();
    let label = elide_middle(&full, max_chars);
    let tooltip = full.clone();

    div()
        .id(id)
        .min_w(px(0.))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_color(color)
        .child(SharedString::from(label))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
}

fn wrapped_text(text: &str, max_chars: usize, color: gpui::Hsla) -> impl IntoElement {
    div().flex().flex_col().text_color(color).children(
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
    // The scheme goes on first. Without it there is nothing to split on, so a
    // bare `s3.example.com/mybucket` used to keep the bucket glued to the host
    // *and* reach the SDK with no scheme — two failures from one typo.
    let normalized = s3core::normalize_endpoint(endpoint);
    let trimmed = normalized.trim_end_matches('/');
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
    secret_optional: bool,
) -> Option<&'static str> {
    if name.is_empty() {
        Some("Cần tên profile")
    } else if access_key.is_empty() {
        Some("Cần access key")
    } else if secret_key.is_empty() && !secret_optional {
        Some("Cần secret key")
    } else if taken.contains(&name) {
        // Names are how profiles are told apart in the sidebar, so a duplicate
        // makes the list unreadable even though the ids stay unique.
        Some("Đã có profile trùng tên")
    } else {
        None
    }
}

fn profile_probe_bucket_ok(bucket: &str, prefix: &str, entries: usize) -> String {
    if prefix.is_empty() {
        format!("Kết nối được tới {bucket}, thấy {entries} mục trang đầu")
    } else {
        format!("Kết nối được tới s3://{bucket}/{prefix}, thấy {entries} mục trang đầu")
    }
}

/// Everything the palette can run. One list so the palette, the shortcut hints
/// and the keyboard handler cannot drift apart — adding a command in one place
/// adds it everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Refresh,
    /// Opens the failure log. Also reachable by clicking the summary in the
    /// status bar, but that one is only there while something has gone wrong.
    Errors,
    /// Rewriting HTTP headers on the selection. Also on the context menu, but
    /// that one needs a row under the pointer; this reaches a selection made
    /// any other way.
    EditHeaders,
    NewTab,
    CloseTab,
    GoToPath,
    ToggleFavorite,
    Recent,
    Buckets,
    Settings,
    /// Version and where the files are. Also at the foot of the sidebar; this
    /// is the way in for someone who lives in the palette.
    About,
    CopyPath,
    CopyKey,
    AclFolderPrivate,
    AclFolderPublic,
    EditText,
    SaveText,
    GoUp,
    Filter,
    NewFolder,
    NewBucket,
    Rename,
    Duplicate,
    Copy,
    Cut,
    Paste,
    SelectAll,
    Share,
    Inspect,
    Preview,
    OpenExternally,
    Download,
    Delete,
    ToggleQueue,
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
            Command::Errors => ("Xem lỗi", ""),
            Command::EditHeaders => ("Sửa header", ""),
            Command::NewTab => ("Tab mới", "⌘T"),
            Command::CloseTab => ("Đóng tab", "⌘W"),
            Command::GoToPath => ("Đi tới đường dẫn", "⌘L"),
            Command::ToggleFavorite => ("Ghim nơi này", ""),
            Command::Recent => ("Gần đây", ""),
            Command::Buckets => ("Tất cả bucket", ""),
            Command::Settings => ("Cài đặt", ""),
            Command::About => ("Giới thiệu", ""),
            Command::CopyPath => ("Chép đường dẫn s3://", ""),
            Command::CopyKey => ("Chép key", ""),
            Command::AclFolderPrivate => ("Thư mục: đặt riêng tư", ""),
            Command::AclFolderPublic => ("Thư mục: ai cũng đọc được", ""),
            Command::EditText => ("Sửa nội dung", ""),
            Command::SaveText => ("Lưu nội dung", ""),
            Command::NewFolder => ("Thư mục mới", "⌘N"),
            Command::NewBucket => ("Bucket mới", "⌘⇧N"),
            Command::Rename => ("Đổi tên", "⌘⏎"),
            Command::Duplicate => ("Nhân bản", ""),
            Command::Copy => ("Chép", "⌘C"),
            Command::Cut => ("Cắt", "⌘X"),
            Command::Paste => ("Dán", "⌘V"),
            Command::SelectAll => ("Chọn tất cả", "⌘A"),
            Command::Share => ("Chia sẻ / presigned URL", ""),
            Command::Inspect => ("Chi tiết", "⌘I"),
            Command::Preview => ("Xem trước", "Space"),
            Command::OpenExternally => ("Mở bằng app ngoài", ""),
            Command::Download => ("Tải xuống", "⌘D"),
            Command::Delete => ("Xoá mục đã chọn", "⌘⌫"),
            Command::ToggleQueue => ("Hàng đợi truyền tải", "⌘J"),
            Command::EmptyBucket => ("Dọn sạch bucket", ""),
            Command::AssumeRole => ("Nhận role (STS AssumeRole)", ""),
            Command::SsoSignIn => ("Đăng nhập AWS SSO", ""),
            Command::NewProfile => ("Profile mới", ""),
        }
    }

    fn all() -> [Command; 38] {
        [
            Command::Refresh,
            Command::GoUp,
            Command::Filter,
            Command::NewFolder,
            Command::NewBucket,
            Command::Rename,
            Command::Duplicate,
            Command::Copy,
            Command::Cut,
            Command::Paste,
            Command::SelectAll,
            Command::Share,
            Command::Inspect,
            Command::Preview,
            Command::OpenExternally,
            Command::Download,
            Command::Delete,
            Command::ToggleQueue,
            Command::EmptyBucket,
            Command::AssumeRole,
            Command::SsoSignIn,
            Command::NewProfile,
            Command::Errors,
            Command::EditHeaders,
            Command::NewTab,
            Command::CloseTab,
            Command::GoToPath,
            Command::ToggleFavorite,
            Command::Recent,
            Command::Buckets,
            Command::Settings,
            Command::About,
            Command::CopyPath,
            Command::CopyKey,
            Command::AclFolderPrivate,
            Command::AclFolderPublic,
            Command::EditText,
            Command::SaveText,
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
            'è' | 'é' | 'ẻ' | 'ẽ' | 'ẹ' | 'ê' | 'ề' | 'ế' | 'ể' | 'ễ' | 'ệ' => {
                'e'
            }
            'ì' | 'í' | 'ỉ' | 'ĩ' | 'ị' => 'i',
            'ò' | 'ó' | 'ỏ' | 'õ' | 'ọ' | 'ô' | 'ồ' | 'ố' | 'ổ' | 'ỗ' | 'ộ' | 'ơ' | 'ờ' | 'ớ'
            | 'ở' | 'ỡ' | 'ợ' => 'o',
            'ù' | 'ú' | 'ủ' | 'ũ' | 'ụ' | 'ư' | 'ừ' | 'ứ' | 'ử' | 'ữ' | 'ự' => {
                'u'
            }
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
        format!("Gồm {folders} thư mục, kể cả nội dung bên trong. ")
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
/// The prefix a key sits directly under, with its trailing slash.
///
/// Used to spot a paste into the folder the items already live in, which would
/// otherwise copy each object onto itself.
fn parent_prefix_of(key: &str) -> String {
    let body = key.trim_end_matches('/');
    match body.rfind('/') {
        Some(ix) => body[..=ix].to_string(),
        None => String::new(),
    }
}

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
            appearance: gpui::WindowAppearance::Dark,
            ui_font: "Inter".into(),
            mono_font: "monospace".into(),
            profiles: Vec::new(),
            active_profile: None,
            store: None,
            places: Places::default(),
            place_store: None,
            settings: Settings::default(),
            settings_store: None,
            settings_open: false,
            about_open: false,
            client: None,
            buckets: Vec::new(),
            bucket_cache: HashMap::new(),
            bucket_created: HashMap::new(),
            buckets_cached: false,
            bucket_cache_bypass: false,
            bucket_filter: String::new(),
            bucket_filter_input: None,
            bucket: Some("demo".into()),
            prefix: String::new(),
            entries,
            visible: Vec::new(),
            continuation: None,
            loading: false,
            loading_more: false,
            connecting: false,
            generation: 0,
            sort: Sort::default(),
            filter: String::new(),
            path_input: None,
            path_dirty: false,
            filter_input: None,
            selection: HashSet::new(),
            cursor: None,
            hovered_row: None,
            anchor: None,
            layout: LayoutMode::List,
            scroll: UniformListScrollHandle::new(),
            status: "test".into(),
            failures: Vec::new(),
            failures_open: false,
            perf: PerfStats::default(),
            transfers: TransferEngine::in_memory().expect("in-memory queue"),
            drawer_open: false,
            expanded_folders: HashSet::new(),

            confirm: None,
            form: None,
            clipboard: None,
            thumbnails: HashMap::new(),
            profiles_open: false,
            sso: None,
            tabs: vec![Tab {
                id: 0,
                state: TabState::default(),
            }],
            active_tab: 0,
            next_tab_id: 1,
            previewing: None,
            opening: None,
            screen: Screen::default(),
            path_suggesting: false,
            path_choice: None,
            preview_task: None,
            completing: None,
            bulk: None,
            search: None,
            palette: None,
            palette_selected: 0,
            palette_scroll: UniformListScrollHandle::new(),
            share: None,
            inspector: None,
            acl_popup_open: false,
            bucket_versioned: false,
            capabilities: None,
            caps_cache: CapabilityCache::default(),
            ticking: false,
            connect_task: None,
            listing_task: None,
            paging_task: None,
            op_task: None,
            open_task: None,
            caps_task: None,
            thumb_task: None,
            tick_task: None,
            search_task: None,
            bulk_task: None,
            complete_task: None,
            _appearance: None,
            _filter_events: None,
            _bucket_filter_events: None,
            _path_events: None,
        };
        browser.resort_and_filter();
        browser
    }

    #[test]
    fn parent_prefix_of_a_key_is_where_a_paste_would_be_a_no_op() {
        assert_eq!(parent_prefix_of("reports/q1.txt"), "reports/");
        assert_eq!(parent_prefix_of("a/b/c.txt"), "a/b/");
        // A key at the root has no parent prefix, which is the empty string and
        // exactly what `self.prefix` holds at the top of a bucket.
        assert_eq!(parent_prefix_of("top.txt"), "");

        // A folder key ends in `/`; its parent is the level above, not itself.
        // Getting this backwards would make pasting a folder into its own
        // parent look like a no-op and silently do nothing.
        assert_eq!(parent_prefix_of("a/b/"), "a/");
        assert_eq!(parent_prefix_of("solo/"), "");
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

    /// The row names in cursor order, so a test can say what is where without
    /// reaching through two levels of indices every time.
    fn row_names(browser: &Browser) -> Vec<&str> {
        browser
            .visible
            .iter()
            .map(|&ix| browser.entries[ix].name.as_str())
            .collect()
    }

    fn selected_names(browser: &Browser) -> Vec<&str> {
        let mut names: Vec<_> = browser
            .visible
            .iter()
            .map(|&ix| &browser.entries[ix])
            .filter(|entry| browser.selection.contains(&entry.key))
            .map(|entry| entry.name.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    #[gpui::test]
    fn arrows_walk_the_rows_and_stop_at_both_ends(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            offline(
                vec![
                    entry("alpha.txt", false, 1),
                    entry("beta.txt", false, 2),
                    entry("gamma.txt", false, 3),
                ],
                cx,
            )
        });

        entity.update(cx, |browser, cx| {
            assert_eq!(
                row_names(browser),
                vec!["alpha.txt", "beta.txt", "gamma.txt"]
            );
            // Nothing under the cursor yet: the first press has to land
            // somewhere rather than move from a position we do not have.
            assert_eq!(browser.cursor_position(), None);

            browser.move_cursor(true, false, cx);
            assert_eq!(browser.cursor_position(), Some(0));
            browser.move_cursor(true, false, cx);
            assert_eq!(browser.cursor_position(), Some(1));

            // The selection travels with the cursor, and only the cursor row
            // stays selected — an arrow key is a move, not an addition.
            assert_eq!(selected_names(browser), vec!["beta.txt"]);

            browser.move_cursor(true, false, cx);
            browser.move_cursor(true, false, cx);
            // Clamped, not wrapping: landing back on the first row after the
            // last one reads as the selection disappearing.
            assert_eq!(browser.cursor_position(), Some(2));

            browser.move_cursor(false, false, cx);
            browser.move_cursor(false, false, cx);
            browser.move_cursor(false, false, cx);
            assert_eq!(browser.cursor_position(), Some(0));
        });
    }

    #[gpui::test]
    fn the_first_press_upwards_starts_at_the_bottom(cx: &mut gpui::TestAppContext) {
        let entity =
            cx.new(|cx| offline(vec![entry("a.txt", false, 1), entry("b.txt", false, 2)], cx));

        entity.update(cx, |browser, cx| {
            // Starting at the top for an upward press would mean the key did
            // nothing visible on a list nobody has touched yet.
            browser.move_cursor(false, false, cx);
            assert_eq!(browser.cursor_position(), Some(1));
        });
    }

    #[gpui::test]
    fn shift_stretches_one_run_and_shrinking_it_deselects(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            offline(
                vec![
                    entry("a.txt", false, 1),
                    entry("b.txt", false, 2),
                    entry("c.txt", false, 3),
                    entry("d.txt", false, 4),
                ],
                cx,
            )
        });

        entity.update(cx, |browser, cx| {
            browser.move_cursor(true, false, cx);
            browser.move_cursor(true, false, cx);
            assert_eq!(selected_names(browser), vec!["b.txt"]);

            browser.move_cursor(true, true, cx);
            browser.move_cursor(true, true, cx);
            assert_eq!(selected_names(browser), vec!["b.txt", "c.txt", "d.txt"]);

            // Coming back has to actually deselect. A range that only ever
            // grows would leave rows selected that are no longer in it, and the
            // next delete would take them along.
            browser.move_cursor(false, true, cx);
            assert_eq!(selected_names(browser), vec!["b.txt", "c.txt"]);

            // Past the anchor the run flips direction rather than emptying.
            browser.move_cursor(false, true, cx);
            browser.move_cursor(false, true, cx);
            assert_eq!(selected_names(browser), vec!["a.txt", "b.txt"]);
        });
    }

    #[gpui::test]
    fn a_plain_arrow_after_a_range_collapses_it(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            offline(
                vec![
                    entry("a.txt", false, 1),
                    entry("b.txt", false, 2),
                    entry("c.txt", false, 3),
                ],
                cx,
            )
        });

        entity.update(cx, |browser, cx| {
            browser.move_cursor(true, false, cx);
            browser.move_cursor(true, true, cx);
            assert_eq!(selected_names(browser), vec!["a.txt", "b.txt"]);

            // Letting go of Shift and pressing again starts a new selection;
            // the old anchor must not keep stretching a run behind it.
            browser.move_cursor(true, false, cx);
            assert_eq!(selected_names(browser), vec!["c.txt"]);
            browser.move_cursor(false, true, cx);
            assert_eq!(selected_names(browser), vec!["b.txt", "c.txt"]);
        });
    }

    #[gpui::test]
    fn the_cursor_stays_on_its_file_when_the_rows_are_renumbered(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            offline(
                vec![
                    entry("apple.txt", false, 1),
                    entry("report.txt", false, 2),
                    entry("zebra.txt", false, 3),
                ],
                cx,
            )
        });

        entity.update(cx, |browser, cx| {
            browser.move_cursor(true, false, cx);
            browser.move_cursor(true, false, cx);
            assert_eq!(browser.cursor.as_deref(), Some("report.txt"));
            assert_eq!(browser.cursor_position(), Some(1));

            // Filtering renumbers the rows. Holding a row number here would
            // slide the cursor onto a different file without anyone touching a
            // key, and the next Enter would open the wrong thing.
            browser.filter = "rep".into();
            browser.resort_and_filter();
            assert_eq!(row_names(browser), vec!["report.txt"]);
            assert_eq!(browser.cursor_position(), Some(0));
            assert_eq!(browser.cursor.as_deref(), Some("report.txt"));

            // Filtered out entirely, the cursor has no row. That is not an
            // error; the next press starts from an end again.
            browser.filter = "zeb".into();
            browser.resort_and_filter();
            assert_eq!(browser.cursor_position(), None);
            browser.move_cursor(true, false, cx);
            assert_eq!(browser.cursor.as_deref(), Some("zebra.txt"));
        });
    }

    #[gpui::test]
    fn clicking_moves_the_cursor_so_arrows_carry_on_from_there(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            offline(
                vec![
                    entry("a.txt", false, 1),
                    entry("b.txt", false, 2),
                    entry("c.txt", false, 3),
                ],
                cx,
            )
        });

        entity.update(cx, |browser, cx| {
            browser.click_row(2, Modifiers::default(), cx);
            assert_eq!(browser.cursor.as_deref(), Some("c.txt"));

            // The whole point: the keyboard picks up where the mouse left off
            // rather than from wherever it last was.
            browser.move_cursor(false, false, cx);
            assert_eq!(selected_names(browser), vec!["b.txt"]);

            // Shift-click is the mouse half of the same range mechanism.
            browser.click_row(0, Modifiers::shift(), cx);
            assert_eq!(selected_names(browser), vec!["a.txt", "b.txt"]);
        });
    }

    #[test]
    fn an_empty_list_says_which_kind_of_empty_it_is() {
        // Nothing loaded and nothing typed: the prefix is empty, full stop.
        assert_eq!(emptiness("", 0), Emptiness::Prefix);
        // Rows loaded, filter hiding all of them. Escape undoes it, so this is
        // the one that gets a button.
        assert_eq!(emptiness("xyz", 12), Emptiness::Filtered);
        // A filter typed against a prefix that was already empty. The filter is
        // not why there is nothing here, and offering to clear it would send
        // someone chasing a cause that does not exist.
        assert_eq!(emptiness("xyz", 0), Emptiness::Prefix);
    }

    #[test]
    fn scrolling_aligns_to_the_end_being_moved_toward() {
        // gpui has no "nearest" strategy, so the direction of travel has to
        // pick the end. Getting it backwards is not a subtle wrongness: the row
        // arrowed onto is thrown to the far edge and a one-row step scrolls a
        // whole page.
        assert_eq!(scroll_edge(true), gpui::ScrollStrategy::Bottom);
        assert_eq!(scroll_edge(false), gpui::ScrollStrategy::Top);
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
        assert_eq!(bandwidth_label(0), "∞");
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
    fn durations_use_the_coarsest_unit_that_still_informs() {
        assert_eq!(format_duration(0), "0 giây");
        assert_eq!(format_duration(45), "45 giây");
        // Past a minute the seconds are noise.
        assert_eq!(format_duration(60), "1 phút");
        assert_eq!(format_duration(3_599), "59 phút");
        // Past an hour, minutes still matter but seconds do not.
        assert_eq!(format_duration(3_600), "1 giờ 0 phút");
        assert_eq!(format_duration(7_380), "2 giờ 3 phút");
        // A transfer measured in days does not need the hours.
        assert_eq!(format_duration(90_000), "1 ngày");
    }

    #[test]
    fn an_age_is_one_unit_and_never_negative() {
        // No number under a minute: this is computed during a repaint and
        // nothing schedules another, so a seconds figure would be true for one
        // instant and wrong from then on.
        assert_eq!(format_age(0), "vừa xong");
        assert_eq!(format_age(59), "vừa xong");
        assert_eq!(format_age(60), "1 phút trước");
        assert_eq!(format_age(300), "5 phút trước");
        assert_eq!(format_age(3_599), "59 phút trước");

        // One unit past the hour, unlike `format_duration`: the second unit
        // there would claim the age is exact when it only moves on a redraw.
        assert_eq!(format_age(3_600), "1 giờ trước");
        assert_eq!(format_age(7_380), "2 giờ trước");
        assert_eq!(format_age(86_399), "23 giờ trước");
        assert_eq!(format_age(90_000), "1 ngày trước");

        // Every arm says "trước". A bare duration in this app already means
        // time remaining — the queue pill three lines away renders "còn 2 phút".
        for seconds in [60, 3_600, 90_000] {
            assert!(format_age(seconds).ends_with("trước"), "{seconds}");
        }

        // The clock moving backwards between the fetch and now. "-3 phút" in
        // the status bar reads as a broken app rather than a broken clock.
        assert_eq!(format_age(-1), "vừa xong");
        assert_eq!(format_age(i64::MIN), "vừa xong");
    }

    #[test]
    fn opaque_tokens_are_elided_not_wrapped() {
        let etag = "529e9abf98dd6f2d15f4f69461565329";
        let short = elide_middle(etag, 28);
        assert_eq!(short.chars().count(), 28);
        // Both ends survive: they are what people compare against a checksum.
        assert!(short.starts_with("529e9abf"), "{short}");
        assert!(short.ends_with("565329"), "{short}");
        assert!(short.contains('…'));

        // Short enough already: untouched, no stray ellipsis.
        assert_eq!(elide_middle("abc", 28), "abc");
        assert_eq!(elide_middle(etag, 32), etag);

        // A budget too small to be meaningful returns the value rather than
        // producing something unreadable.
        assert_eq!(elide_middle(etag, 3), etag);

        // Multi-byte input must not be cut mid-character.
        let viet = "khoá-bí-mật-rất-dài-không-nên-xuống-dòng";
        let cut = elide_middle(viet, 12);
        assert_eq!(cut.chars().count(), 12);
    }

    #[gpui::test]
    fn the_sidebar_filter_is_accent_blind_like_the_rest(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| offline(Vec::new(), cx));

        entity.update(cx, |browser, _| {
            browser.buckets = vec!["ảnh-2026".into(), "reports".into(), "logs".into()];

            // Nothing typed shows everything.
            assert_eq!(browser.visible_buckets().len(), 3);

            // Typed without diacritics, which is how Vietnamese gets typed at a
            // keyboard more often than not.
            browser.bucket_filter = "anh".into();
            assert_eq!(
                browser.visible_buckets(),
                vec![SharedString::from("ảnh-2026")]
            );

            // And with them.
            browser.bucket_filter = "Ảnh".into();
            assert_eq!(browser.visible_buckets().len(), 1);

            browser.bucket_filter = "og".into();
            assert_eq!(browser.visible_buckets(), vec![SharedString::from("logs")]);
        });
    }

    #[gpui::test]
    fn the_list_filter_is_accent_blind_too(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            offline(
                vec![entry("báo-cáo.txt", false, 1), entry("notes.txt", false, 2)],
                cx,
            )
        });

        entity.update(cx, |browser, _| {
            // Same rule as the search that fills this list, so filtering a set
            // of results by the query that found them cannot hide any of them.
            browser.filter = "bao".into();
            browser.resort_and_filter();
            assert_eq!(row_names(browser), vec!["báo-cáo.txt"]);
        });
    }

    #[test]
    fn a_stopped_scan_never_reads_as_a_finished_one() {
        let line = |complete, capped, running| {
            search_summary(SearchProgress {
                found: 12,
                scanned: 4000,
                requests: 4,
                complete,
                capped,
                running,
            })
        };

        // The whole point of saying anything at all. "Không tìm thấy" from a
        // scan that covered a tenth of the bucket is a claim the app has no
        // basis for, and it is the claim that sends someone looking for a file
        // somewhere else entirely.
        assert!(line(true, false, false).contains("xong"));
        assert!(line(false, false, true).contains("đang quét"));
        assert!(line(false, false, false).contains("đã dừng"));
        assert!(line(false, true, false).contains("chạm trần"));

        // A cap reached on the very last page did not cover the bucket, whatever
        // the continuation token happened to say.
        assert!(line(true, true, false).contains("chạm trần"));

        // The request count is there because that is the line item on the bill,
        // not the key count.
        let line = search_summary(SearchProgress {
            found: 0,
            scanned: 9000,
            requests: 9,
            complete: false,
            capped: false,
            running: true,
        });
        assert!(line.contains("9 yêu cầu"), "{line}");
        assert!(line.contains("9000 mục"), "{line}");
        assert!(line.starts_with("0 kết quả"), "{line}");
    }

    #[test]
    fn flattening_makes_one_paragraph_of_a_cause_chain() {
        // What `anyhow` prints: a message, then indented `Caused by:` lines.
        // Re-wrapping that at a column leaves a stub after every original line
        // break, which reads as a broken renderer rather than as an error.
        let raw = "ListBuckets failed\n\nCaused by:\n    0: service error\n    1: bad key";
        assert_eq!(
            flatten(raw),
            "ListBuckets failed Caused by: 0: service error 1: bad key"
        );
        // Already one line: nothing to do, and no trailing space either.
        assert_eq!(flatten("một dòng"), "một dòng");
        assert_eq!(flatten("  thừa   khoảng trắng  "), "thừa khoảng trắng");
    }

    #[test]
    fn repeated_failures_are_grouped_newest_first() {
        let failures = vec![
            Failure {
                summary: "Sai khoá".into(),
                detail: "SignatureDoesNotMatch".into(),
                fix: Some(Fix::EditProfile),
                at: 1,
            },
            Failure {
                summary: "Endpoint chết".into(),
                detail: "Connection refused".into(),
                fix: Some(Fix::Retry),
                at: 2,
            },
            Failure {
                summary: "Sai khoá".into(),
                detail: "SignatureDoesNotMatch".into(),
                fix: Some(Fix::EditProfile),
                at: 3,
            },
        ];

        let cards = failure_cards(&failures);

        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].summary.as_ref(), "Sai khoá");
        assert_eq!(cards[0].count, 2);
        assert_eq!(cards[0].at, 3);
        assert_eq!(cards[1].summary.as_ref(), "Endpoint chết");
        assert_eq!(cards[1].count, 1);
    }

    #[test]
    fn wrapping_never_splits_a_word() {
        // The bug this exists for: gpui breaks before any non-ASCII character,
        // turning "cấp" into "c" / "ấp". Every line here must be whole words.
        let text = "Token này có thể chỉ có quyền trên một bucket cụ thể, với R2 đó là cách cấp quyền được khuyến nghị.";
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
        // A scheme is put on before splitting. Without that step there is
        // nothing to split on, so the bucket stayed glued to the host and the
        // SDK got a URL it answers with `dispatch failure`.
        assert_eq!(
            split_endpoint("localhost:9000"),
            ("http://localhost:9000".into(), None)
        );
        assert_eq!(
            split_endpoint("s3.example.com/mybucket"),
            ("https://s3.example.com".into(), Some("mybucket".into()))
        );
    }

    #[test]
    fn the_sidebar_goes_dead_with_the_client_not_with_the_profile() {
        // Connected: nothing greyed out, no row carrying a tooltip.
        assert_eq!(sidebar_blocked_reason(true), None);

        // Not connected. Keyed on the client rather than on `active_profile`
        // because a connect that fails after the profile is chosen leaves a
        // profile selected and no client — and every row lit, promising work
        // that `open` and `refresh_buckets` both refuse to do.
        assert_eq!(
            sidebar_blocked_reason(false),
            Some("Chưa kết nối tới profile nào")
        );
    }

    #[test]
    fn profile_validation_catches_every_way_the_form_can_be_wrong() {
        // The happy path.
        assert_eq!(validate_profile("R2", "AKIA", "secret", &[], false), None);

        // Each required field, reported specifically rather than as one vague
        // "invalid" — the point of the message is to say which box to fill.
        assert_eq!(
            validate_profile("", "AKIA", "secret", &[], false),
            Some("Cần tên profile")
        );
        assert_eq!(
            validate_profile("R2", "", "secret", &[], false),
            Some("Cần access key")
        );
        assert_eq!(
            validate_profile("R2", "AKIA", "", &[], false),
            Some("Cần secret key")
        );
        assert_eq!(validate_profile("R2", "AKIA", "", &[], true), None);

        // Duplicate names make the sidebar unreadable even though ids stay
        // unique underneath.
        assert_eq!(
            validate_profile("R2", "AKIA", "secret", &["R2"], false),
            Some("Đã có profile trùng tên")
        );
        // A different name alongside an existing one is fine.
        assert_eq!(
            validate_profile("B2", "AKIA", "secret", &["R2"], false),
            None
        );
    }

    #[test]
    fn profile_probe_names_the_bucket_scope_it_actually_tested() {
        assert_eq!(
            profile_probe_bucket_ok("photos", "", 3),
            "Kết nối được tới photos, thấy 3 mục trang đầu"
        );
        assert_eq!(
            profile_probe_bucket_ok("photos", "2026/", 1),
            "Kết nối được tới s3://photos/2026/, thấy 1 mục trang đầu"
        );
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
        assert_eq!(
            parsed.mfa_serial.as_deref(),
            Some("arn:aws:iam::123:mfa/mai")
        );
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
        assert_eq!(all.len(), 38);
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
    fn the_version_line_names_the_program_and_a_missing_path_says_so() {
        // The number alone identifies nothing when it arrives in a bug report
        // without the program it came from.
        let line = version_line();
        assert!(line.starts_with("s3browser"), "{line}");
        assert!(line.contains(env!("CARGO_PKG_VERSION")), "{line}");

        // A folder the app cannot locate still says so out loud. An empty row
        // would leave the reader unable to tell "there is no such folder" from
        // "this dialog is broken".
        assert_eq!(dir_label(None, 44), "Không rõ");

        // A long path keeps both ends: the front says whose home it is under,
        // the last segment says which of the two folders it is.
        let shown = dir_label(
            Some(Path::new(
                "/Users/ai/Library/Application Support/s3browser/crashes",
            )),
            44,
        );
        assert!(shown.starts_with("/Users/ai/"), "{shown}");
        assert!(shown.ends_with("crashes"), "{shown}");
        assert!(shown.chars().count() <= 44, "{shown}");
    }

    #[test]
    fn a_crash_folder_that_does_not_exist_yet_reveals_its_parent() {
        let root = std::env::temp_dir().join(format!("s3b-about-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let crashes = root.join("crashes");

        // Nothing has crashed, so the folder named in the dialog is not there.
        // Opening the path itself would fail with the system's own words about
        // a missing file, which reads as a broken button rather than as good
        // news.
        assert_eq!(revealable(&crashes), root.as_path());

        // Once it exists it is what opens; the walk up must not overshoot the
        // folder holding the reports.
        std::fs::create_dir_all(&crashes).unwrap();
        assert_eq!(revealable(&crashes), crashes.as_path());

        _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_bulk_edit_never_includes_a_folder(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            offline(
                vec![
                    entry("reports", true, 0),
                    entry("a.txt", false, 1),
                    entry("b.txt", false, 2),
                ],
                cx,
            )
        });

        entity.update(cx, |browser, _| {
            browser.selection.insert("reports/".into());
            browser.selection.insert("a.txt".into());
            browser.selection.insert("b.txt".into());

            // A folder is a common prefix, not an object. Sending it would ask
            // the provider to copy a key that does not exist.
            assert_eq!(browser.selected_object_keys(), vec!["a.txt", "b.txt"]);

            browser.selection.clear();
            assert!(browser.selected_object_keys().is_empty());
        });
    }

    #[gpui::test]
    fn inspector_opens_on_the_first_object_in_a_multi_selection(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            offline(
                vec![
                    entry("reports", true, 0),
                    entry("a.txt", false, 1),
                    entry("b.txt", false, 2),
                ],
                cx,
            )
        });

        entity.update(cx, |browser, cx| {
            browser.selection.insert("reports/".into());
            browser.selection.insert("a.txt".into());
            browser.selection.insert("b.txt".into());

            browser.open_inspector(cx);

            assert_eq!(
                browser
                    .inspector
                    .as_ref()
                    .map(|inspector| inspector.key.as_str()),
                Some("a.txt")
            );
            assert_eq!(browser.selected_object_keys(), vec!["a.txt", "b.txt"]);
        });
    }

    #[gpui::test]
    fn open_inspector_follows_the_newly_selected_object(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| {
            offline(
                vec![
                    entry("reports", true, 0),
                    entry("a.txt", false, 1),
                    entry("b.txt", false, 2),
                ],
                cx,
            )
        });

        entity.update(cx, |browser, cx| {
            browser.selection.insert("a.txt".into());
            browser.cursor = Some("a.txt".into());
            browser.open_inspector(cx);
            assert_eq!(
                browser
                    .inspector
                    .as_ref()
                    .map(|inspector| inspector.key.as_str()),
                Some("a.txt")
            );

            browser.click_row(2, Modifiers::default(), cx);
            assert_eq!(
                browser
                    .inspector
                    .as_ref()
                    .map(|inspector| inspector.key.as_str()),
                Some("b.txt")
            );
        });
    }

    #[test]
    fn acl_panel_names_the_multi_file_target() {
        assert_eq!(acl_target_label(0), "Áp dụng cho object này");
        assert_eq!(acl_target_label(1), "Áp dụng cho object này");
        assert_eq!(acl_target_label(3), "Áp dụng cho 3 tệp đã chọn");
    }

    #[test]
    fn acl_unavailable_copy_separates_bucket_support_from_permissions() {
        assert_eq!(
            acl_unavailable_copy(Some(Support::No)).map(|(summary, _)| summary),
            Some("Bucket này đã tắt ACL")
        );
        assert_eq!(
            acl_unavailable_copy(Some(Support::Forbidden)).map(|(summary, _)| summary),
            Some("Token không có quyền ACL")
        );
        assert!(acl_unavailable_copy(Some(Support::Yes)).is_none());
        assert!(acl_unavailable_copy(None).is_none());
    }

    #[test]
    fn acl_permission_table_expands_full_control() {
        let acl = ObjectAcl {
            owner: "owner-1".into(),
            grants: vec![
                s3core::AclGrant {
                    grantee: "owner-1".into(),
                    permission: "FULL_CONTROL".into(),
                    public: false,
                },
                s3core::AclGrant {
                    grantee: "Mọi người".into(),
                    permission: "READ".into(),
                    public: true,
                },
            ],
        };

        let rows = acl_permission_rows(&acl);
        assert_eq!(
            rows.first(),
            Some(&AclPermissionRow {
                grantee: "owner-1".into(),
                read: true,
                write: true,
                full: true,
                public: false,
            })
        );
        assert!(rows.iter().any(|row| row.grantee == "Mọi người"
            && row.read
            && !row.write
            && !row.full
            && row.public));
    }

    #[gpui::test]
    fn acl_actions_do_not_start_when_bucket_has_acls_disabled(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| offline(vec![entry("a.txt", false, 1)], cx));

        entity.update(cx, |browser, cx| {
            browser.capabilities = Some(Capabilities {
                versioning: Support::Yes,
                tagging: Support::Yes,
                lifecycle: Support::Yes,
                object_lock: Support::Yes,
                acl: Support::No,
            });
            browser.selection.insert("a.txt".into());

            browser.set_acl("public-read", cx);

            assert!(browser.bulk.is_none());
            assert_eq!(browser.failures.len(), 1);
            assert_eq!(browser.failures[0].summary, "Bucket này đã tắt ACL");
        });
    }

    #[test]
    fn preview_geometry_stays_fixed() {
        assert_eq!(PREVIEW_WIDTH, 980.);
        assert_eq!(PREVIEW_HEIGHT, 560.);
        assert_eq!(PREVIEW_SIDE_WIDTH, 276.);
        assert_eq!(PREVIEW_BODY_HEIGHT, PREVIEW_HEIGHT - PREVIEW_TITLE_HEIGHT);
        let body_height = std::hint::black_box(PREVIEW_BODY_HEIGHT);
        assert!(body_height > 500.);
    }

    #[test]
    fn a_long_field_label_widens_the_column_instead_of_wrapping() {
        // `Content-Disposition` wrapped onto two lines at the old fixed width
        // and dragged its own input out of line with every other row.
        let short = form_label_width(&FormKind::NewFolder);
        let long = form_label_width(&FormKind::EditHeaders(vec!["a".into()]));

        // A one-word label keeps the old floor rather than shrinking the column
        // to nothing.
        assert_eq!(short, 84.);
        assert!(long > 120., "{long}");
    }

    #[gpui::test]
    fn switching_tabs_puts_everything_back_where_it_was(cx: &mut gpui::TestAppContext) {
        let entity =
            cx.new(|cx| offline(vec![entry("a.txt", false, 1), entry("b.txt", false, 2)], cx));

        entity.update(cx, |browser, _| {
            browser.prefix = "photos/2026/".into();
            browser.selection.insert("b.txt".into());
            browser.cursor = Some("b.txt".into());
            browser.filter = "b".into();
            browser.sort = Sort {
                key: SortKey::Size,
                ..browser.sort
            };

            // Leaving the tab lifts all of that out, and leaves the live state
            // empty rather than half of the old tab showing through.
            let saved = browser.capture_tab();
            assert!(browser.entries.is_empty());
            assert!(browser.selection.is_empty());
            assert!(browser.prefix.is_empty());
            assert_eq!(saved.entries.len(), 2);

            // Coming back is not "load the folder again": the rows, the
            // selection, the cursor, the filter and the sort are the same ones.
            browser.apply_tab_state(saved);
            assert_eq!(browser.prefix, "photos/2026/");
            assert_eq!(browser.entries.len(), 2);
            assert!(browser.selection.contains("b.txt"));
            assert_eq!(browser.cursor.as_deref(), Some("b.txt"));
            assert_eq!(browser.filter, "b");
            assert_eq!(browser.sort.key, SortKey::Size);
        });
    }

    #[test]
    fn a_pasted_path_is_read_in_every_shape_it_arrives_in() {
        let path = |text: &str| parse_s3_path(text).map(|p| (p.bucket, p.prefix, p.region));

        // The canonical form, and the same thing with the scheme left off —
        // both are what ends up in a clipboard.
        assert_eq!(
            path("s3://demo-bucket/photos/2026/"),
            Some(("demo-bucket".into(), "photos/2026/".into(), None))
        );
        assert_eq!(
            path("demo-bucket/photos/2026"),
            Some(("demo-bucket".into(), "photos/2026/".into(), None))
        );

        // The trailing slash is added, because a prefix listing without it
        // returns nothing and nobody should have to know that.
        assert_eq!(
            path("s3://demo-bucket/photos"),
            Some(("demo-bucket".into(), "photos/".into(), None))
        );

        // Just a bucket, with or without the slash.
        assert_eq!(
            path("s3://demo-bucket"),
            Some(("demo-bucket".into(), String::new(), None))
        );
        assert_eq!(
            path("s3://demo-bucket/"),
            Some(("demo-bucket".into(), String::new(), None))
        );

        // Whitespace from a copy-paste.
        assert_eq!(
            path("  s3://demo-bucket/x/  "),
            Some(("demo-bucket".into(), "x/".into(), None))
        );

        // The `bucket@region` form other tools print. Kept, not dropped: the
        // endpoint comes from the profile, so a path naming another region
        // would quietly go somewhere else.
        assert_eq!(
            path("s3://demo-bucket@eu-west-1/x/"),
            Some(("demo-bucket".into(), "x/".into(), Some("eu-west-1".into())))
        );

        // Nothing to go to.
        assert_eq!(path(""), None);
        assert_eq!(path("s3://"), None);
        assert_eq!(path("s3:///photos/"), None);
        // A stray `@` is a typo, and guessing which half is the bucket would
        // send someone to a bucket they did not name.
        assert_eq!(path("s3://demo-bucket@/x"), None);
        assert_eq!(path("s3://@eu-west-1/x"), None);
    }

    #[test]
    fn editing_is_refused_for_anything_the_preview_cut_short() {
        let limit = 8 * 1024 * 1024;

        // All of it is here: safe to edit and write back.
        assert!(preview_holds_whole_object(0, limit));
        assert!(preview_holds_whole_object(1024, limit));
        // Exactly the limit is still all of it.
        assert!(preview_holds_whole_object(limit as i64, limit));

        // One byte over and the preview stopped short. Saving that back deletes
        // everything past the cut, quietly, with no way back on a bucket that
        // keeps no versions. This is the one assertion in this file that stands
        // between a text editor and data loss.
        assert!(!preview_holds_whole_object(limit as i64 + 1, limit));
        assert!(!preview_holds_whole_object(2 * 1024 * 1024 * 1024, limit));

        // A size the listing could not report is not a size worth trusting.
        assert!(!preview_holds_whole_object(-1, limit));
    }

    #[test]
    fn copying_a_selection_gives_one_key_per_line() {
        let keys = vec!["reports/q1.txt".to_string(), "photos/".to_string()];

        // The form every other tool takes, folders included: a prefix is a
        // perfectly good thing to paste into `aws s3 sync`.
        assert_eq!(
            location_text("demo", &keys, true),
            "s3://demo/reports/q1.txt\ns3://demo/photos/"
        );

        // And the bare form, for code that already knows the bucket.
        assert_eq!(
            location_text("demo", &keys, false),
            "reports/q1.txt\nphotos/"
        );

        // One key is one line with nothing appended.
        assert_eq!(
            location_text("demo", &keys[..1], true),
            "s3://demo/reports/q1.txt"
        );
    }

    #[test]
    fn a_type_tile_shrinks_its_letters_rather_than_cutting_them() {
        // Three characters — png, pdf, mp4 — is the common case and gets the
        // roomiest size that still clears the tile's edges.
        assert_eq!(kind_label_size(3), 10.);
        // One digit, which is what a selection count usually is, gets the
        // largest of all rather than being drawn at extension size.
        assert_eq!(kind_label_size(1), 11.);
        // The reason the function exists: six characters comes out small but
        // whole. Truncating `SQLITE` to `SQLI` would name a type the file does
        // not have, in a tile whose entire job is to name the type.
        assert!(kind_label_size(6) >= 7.);
        // Never bigger for being longer, at any length including past the
        // point `type_badge_of` stops handing anything over.
        for length in 1..10 {
            assert!(
                kind_label_size(length) <= kind_label_size(length - 1),
                "{length} chữ lại to hơn {} chữ",
                length - 1
            );
        }
    }

    #[test]
    fn the_type_chip_shows_an_extension_and_nothing_else() {
        assert_eq!(
            type_badge(&entry("anh.png", false, 1)).map(|b| b.to_string()),
            Some("PNG".to_string())
        );
        assert_eq!(
            type_badge(&entry("bang.csv", false, 1)).map(|b| b.to_string()),
            Some("CSV".to_string())
        );

        // A folder has no type, and an empty chip beside its name is a smudge.
        assert_eq!(type_badge(&entry("reports", true, 0)), None);
        // Nothing after a dot to show.
        assert_eq!(type_badge(&entry("README", false, 1)), None);
        // The half of a name after a stray dot is not a file type. Showing
        // `BACKUP2026` here would be inventing a type nobody has.
        assert_eq!(type_badge(&entry("archive.backup2026", false, 1)), None);
        // Nor is punctuation.
        assert_eq!(
            type_badge(&entry("a.tar.gz", false, 1)).map(|b| b.to_string()),
            Some("GZ".to_string())
        );
        assert_eq!(type_badge(&entry("weird.a-b", false, 1)), None);
    }

    #[test]
    fn only_the_order_s3_already_returns_is_free() {
        // `ListObjectsV2` gives lexicographic order and nothing else, so this is
        // the one sort that a half-loaded prefix answers correctly.
        assert!(!needs_complete_listing(Sort {
            key: SortKey::Name,
            ascending: true
        }));

        // Everything else is a claim about keys that have not been fetched.
        // Name *descending* included: the last key lexicographically is the one
        // furthest from what has arrived.
        assert!(needs_complete_listing(Sort {
            key: SortKey::Name,
            ascending: false
        }));
        assert!(needs_complete_listing(Sort {
            key: SortKey::Size,
            ascending: true
        }));
        assert!(needs_complete_listing(Sort {
            key: SortKey::Modified,
            ascending: false
        }));
    }

    #[gpui::test]
    fn a_sort_over_part_of_a_prefix_says_so(cx: &mut gpui::TestAppContext) {
        let entity =
            cx.new(|cx| offline(vec![entry("a.txt", false, 1), entry("b.txt", false, 2)], cx));

        entity.update(cx, |browser, _| {
            // Everything here: any sort is exact, so the line has nothing to
            // add over the count in the footer under the list.
            browser.sort = Sort {
                key: SortKey::Size,
                ascending: true,
            };
            assert_eq!(browser.listing_summary(), "");

            // More pages outstanding. Sorting by size now answers "the largest
            // of the ones that happen to have arrived", and the old code said
            // nothing at all about that.
            browser.continuation = Some("token".into());
            assert!(
                browser
                    .listing_summary()
                    .contains("sắp xếp trên phần đã tải"),
                "{}",
                browser.listing_summary()
            );

            // Back to the order S3 returns, and there is nothing to warn about:
            // the pages arrive already in this order.
            browser.sort = Sort {
                key: SortKey::Name,
                ascending: true,
            };
            assert_eq!(browser.listing_summary(), "");
        });
    }

    #[test]
    fn the_number_keys_pick_a_tab_and_nine_means_the_last_one() {
        assert_eq!(tab_shortcut("1", 3), Some(0));
        assert_eq!(tab_shortcut("3", 3), Some(2));
        // Past the end does nothing rather than jumping somewhere arbitrary.
        assert_eq!(tab_shortcut("4", 3), None);
        // 9 has to go somewhere with three tabs open, and the end is where every
        // browser sends it.
        assert_eq!(tab_shortcut("9", 3), Some(2));
        assert_eq!(tab_shortcut("9", 12), Some(11));
        // There is no tab zero.
        assert_eq!(tab_shortcut("0", 3), None);
    }

    #[test]
    fn a_tab_is_named_after_the_deepest_part_of_where_it_is() {
        let bucket = SharedString::from("demo-bucket");

        // At the root there is no segment, so the bucket names the tab.
        assert_eq!(Tab::title(Some(&bucket), ""), "demo-bucket");
        // Deeper, the last segment is what tells two tabs in the same bucket
        // apart — "demo-bucket" three times over says nothing.
        assert_eq!(Tab::title(Some(&bucket), "photos/2026/"), "2026");
        // With or without the trailing slash.
        assert_eq!(Tab::title(Some(&bucket), "photos/2026"), "2026");
        // A tab that is not anywhere yet still needs a name.
        assert_eq!(Tab::title(None, ""), "Trống");
    }

    #[test]
    fn a_batch_header_edit_says_how_many_it_will_change() {
        // Once the dialog is open, setting a header on one object and on four
        // hundred look exactly the same.
        assert_eq!(
            FormKind::EditHeaders(vec!["a".into()]).title(),
            "Sửa header"
        );
        assert_eq!(
            FormKind::EditHeaders(vec!["a".into(), "b".into(), "c".into()]).title(),
            "Sửa header cho 3 mục"
        );
    }

    #[test]
    fn a_quoted_field_keeps_its_commas_and_newlines() {
        // The reason this parser exists rather than a `split(',')`. Every one of
        // these is ordinary in a CSV exported from a spreadsheet, and every one
        // of them breaks the naive version.
        let rows = parse_rows("a,\"b,c\",d\n", ',');
        assert_eq!(rows, vec![vec!["a", "b,c", "d"]]);

        // Doubled quotes are one literal quote.
        let rows = parse_rows("\"she said \"\"hi\"\"\",x\n", ',');
        assert_eq!(rows, vec![vec!["she said \"hi\"", "x"]]);

        // A newline inside quotes is part of the field, not the end of the row.
        let rows = parse_rows("\"line one\nline two\",x\n", ',');
        assert_eq!(rows, vec![vec!["line one\nline two", "x"]]);
    }

    #[test]
    fn row_endings_and_the_last_line_are_both_handled() {
        // CRLF is what a Windows export produces, and a stray `\r` at the end of
        // every field would show up in every cell.
        assert_eq!(
            parse_rows("a,b\r\nc,d\r\n", ','),
            vec![vec!["a", "b"], vec!["c", "d"]]
        );

        // A file that ends without a newline still has a last row...
        assert_eq!(
            parse_rows("a,b\nc,d", ','),
            vec![vec!["a", "b"], vec!["c", "d"]]
        );
        // ...and one that ends with a newline does not gain an empty one.
        assert_eq!(parse_rows("a,b\n", ','), vec![vec!["a", "b"]]);

        // An empty field is a field.
        assert_eq!(parse_rows("a,,c\n", ','), vec![vec!["a", "", "c"]]);
    }

    #[test]
    fn a_ragged_file_still_lines_up() {
        // Short rows are common and are not a reason to refuse to show the file.
        // Without padding, the columns after the gap slide left by one and the
        // table reads as though the data were in different columns.
        let table = parse_table("a,b,c\n1,2\n", ',');
        assert_eq!(table.headers, vec!["a", "b", "c"]);
        assert_eq!(table.rows, vec![vec!["1", "2", ""]]);
    }

    #[test]
    fn what_is_left_out_is_counted_not_dropped() {
        // A preview that stops at two hundred rows and says nothing looks like a
        // file with two hundred rows in it.
        let mut text = String::from("h\n");
        for i in 0..TABLE_ROWS + 5 {
            text.push_str(&format!("{i}\n"));
        }
        let table = parse_table(&text, ',');
        assert_eq!(table.rows.len(), TABLE_ROWS);
        assert_eq!(table.hidden_rows, 5);
        assert_eq!(hidden_note(&table), "còn 5 dòng nữa");

        let wide: String = (0..TABLE_COLUMNS + 3)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let table = parse_table(&wide, ',');
        assert_eq!(table.headers.len(), TABLE_COLUMNS);
        assert_eq!(table.hidden_columns, 3);
        assert_eq!(hidden_note(&table), "còn 3 cột nữa");
    }

    #[test]
    fn a_column_is_as_wide_as_its_widest_cell() {
        // Equal-width columns waste the panel on a table of short ids and elide
        // the one column that has anything in it.
        let table = parse_table("id,ghi chú\n1,một dòng ghi chú dài\n", ',');
        let widths = table_column_widths(&table);
        assert!(widths[1] > widths[0], "{widths:?}");
        // And nothing runs away: one essay in a cell must not push the rest off
        // the panel.
        let table = parse_table(&format!("a,b\n{},x\n", "y".repeat(500)), ',');
        let widths = table_column_widths(&table);
        assert!(widths[0] < 250., "{widths:?}");
    }

    #[test]
    fn only_supported_preview_kinds_are_drawn_here() {
        // Deliberately the whole of it. A PDF would mean a rasteriser bundled
        // and signed, and video means native playback surfaces. Everything
        // outside images and readable text lands on
        // `None`, which is not a dead end — it is the panel that hands the file
        // to the app on the machine.
        for name in [
            "bai-hat.mp3",
            "hop-dong.pdf",
            "phim.mp4",
            "clip.MOV",
            "luu-tru.zip",
            "so-lieu.xlsx",
            "index.html",
            "index.htm",
            "khong-duoi",
        ] {
            assert_eq!(preview_kind(name, None), PreviewKind::None, "{name}");
        }
        assert_eq!(
            preview_kind("khong-duoi", Some("video/quicktime")),
            PreviewKind::None
        );

        // And what the app *can* draw still gets drawn.
        assert!(matches!(preview_kind("anh.png", None), PreviewKind::Image));
        assert!(matches!(
            preview_kind("ghi-chu.txt", None),
            PreviewKind::Text
        ));
    }

    #[test]
    fn the_path_box_highlight_walks_off_both_ends_into_the_text() {
        // `None` is where what-you-typed lives, so both ends land on it rather
        // than wrapping. Wrapping would make it impossible to get back to your
        // own text without deleting a character.
        assert_eq!(next_choice(None, true, 3), Some(0));
        assert_eq!(next_choice(Some(0), true, 3), Some(1));
        assert_eq!(next_choice(Some(2), true, 3), None);

        // Up from the text goes to the *last* row — the one nearest it on
        // screen, since the list hangs below the box.
        assert_eq!(next_choice(None, false, 3), Some(2));
        assert_eq!(next_choice(Some(2), false, 3), Some(1));
        assert_eq!(next_choice(Some(0), false, 3), None);

        // One suggestion: down onto it, down again back to the text.
        assert_eq!(next_choice(None, true, 1), Some(0));
        assert_eq!(next_choice(Some(0), true, 1), None);
    }

    #[test]
    fn a_deep_path_keeps_its_tail_and_drops_its_middle() {
        let crumbs = |names: &[&str]| -> Vec<(SharedString, String)> {
            let mut out = Vec::new();
            let mut acc = String::new();
            for name in names {
                acc.push_str(name);
                acc.push('/');
                out.push((SharedString::from(name.to_string()), acc.clone()));
            }
            out
        };

        // Short enough to draw whole: nothing dropped, nothing to say about it.
        let (collapsed, shown) = visible_crumbs(crumbs(&["a", "b", "c"]), 3);
        assert!(!collapsed);
        assert_eq!(shown.len(), 3);

        // One over, and the *oldest* goes. What survives is where you are and
        // the way back — the reverse of what an overflow_hidden row does.
        let (collapsed, shown) = visible_crumbs(crumbs(&["a", "b", "c", "d"]), 3);
        assert!(collapsed);
        assert_eq!(
            shown
                .iter()
                .map(|(name, _)| name.as_ref())
                .collect::<Vec<_>>(),
            ["b", "c", "d"]
        );

        // The real case: seven levels, and the last three are still the last
        // three. The prefix each one carries stays correct, or clicking a crumb
        // would navigate somewhere that is not what it says.
        let deep = crumbs(&[
            "du-an",
            "khach-hang",
            "2026",
            "quy-3",
            "thang-8",
            "hop-dong",
            "ban-nhap",
        ]);
        let (collapsed, shown) = visible_crumbs(deep, 3);
        assert!(collapsed);
        assert_eq!(
            shown
                .iter()
                .map(|(name, _)| name.as_ref())
                .collect::<Vec<_>>(),
            ["thang-8", "hop-dong", "ban-nhap"]
        );
        assert_eq!(
            shown.last().unwrap().1,
            "du-an/khach-hang/2026/quy-3/thang-8/hop-dong/ban-nhap/"
        );

        // A bucket root has no segments at all, and must not report a collapse
        // — an `…` with nothing behind it is a lie with a click target on it.
        let (collapsed, shown) = visible_crumbs(Vec::new(), 3);
        assert!(!collapsed);
        assert!(shown.is_empty());

        // `keep` of zero would otherwise drop everything and leave only `…`.
        let (collapsed, shown) = visible_crumbs(crumbs(&["a", "b"]), 0);
        assert!(collapsed);
        assert_eq!(shown.len(), 1);
    }

    #[test]
    fn a_file_with_nothing_to_draw_still_says_what_it_is() {
        // The panel names the type from the key, because there is no listing
        // row in hand — and falls back rather than showing an empty tile.
        assert_eq!(type_badge_of("hop-dong.pdf").unwrap(), "PDF");
        assert_eq!(type_badge_of("phim.mp4").unwrap(), "MP4");
        assert_eq!(type_badge_of("khong-duoi"), None);
        // A suffix is not an extension: `sao-luu.backup2026` has no type.
        assert_eq!(type_badge_of("sao-luu.backup2026"), None);
    }

    #[test]
    fn grid_file_names_do_not_repeat_the_extension() {
        assert_eq!(name_without_type("photo.final.png"), "photo.final");
        assert_eq!(name_without_type("anh.WEBP"), "anh");
        assert_eq!(name_without_type("README"), "README");
        assert_eq!(name_without_type(".gitignore"), ".gitignore");
        assert_eq!(
            name_without_type("sao-luu.backup2026"),
            "sao-luu.backup2026"
        );
    }

    #[test]
    fn grid_rows_cover_the_last_partial_line() {
        assert_eq!(grid_row_count(0), 0);
        assert_eq!(grid_row_count(1), 1);
        assert_eq!(grid_row_count(GRID_COLUMNS), 1);
        assert_eq!(grid_row_count(GRID_COLUMNS + 1), 2);
    }

    #[test]
    fn grid_reveal_staggers_from_the_end_of_the_visible_range() {
        assert_eq!(stagger_from_end(11, 12, 17), 0);
        assert_eq!(stagger_from_end(10, 12, 17), MOTION_STAGGER_MS);
        assert_eq!(stagger_from_end(0, 12, 17), 11 * MOTION_STAGGER_MS);
        assert_eq!(stagger_from_end(0, 40, 17), 17 * MOTION_STAGGER_MS);
    }

    #[gpui::test]
    fn scoped_bucket_shortcuts_stay_on_the_active_profile(cx: &mut gpui::TestAppContext) {
        let entity = cx.new(|cx| offline(Vec::new(), cx));
        entity.update(cx, |browser, _| {
            browser.profiles.push(StoredProfile {
                id: "p1".into(),
                name: "P1".into(),
                endpoint: None,
                region: "us-east-1".into(),
                path_style: false,
                relaxed_checksums: false,
                access_key: "key".into(),
            });
            browser.active_profile = Some(0);
            browser.buckets = vec!["listed".into()];
            browser.places.visit(Place {
                profile: "p1".into(),
                bucket: "listed".into(),
                prefix: String::new(),
                at: 1,
            });
            browser.places.visit(Place {
                profile: "p1".into(),
                bucket: "direct-only".into(),
                prefix: String::new(),
                at: 2,
            });
            browser.places.visit(Place {
                profile: "p2".into(),
                bucket: "wrong-profile".into(),
                prefix: String::new(),
                at: 3,
            });

            let buckets: Vec<_> = browser
                .known_bucket_shortcuts()
                .into_iter()
                .map(|bucket| bucket.to_string())
                .collect();
            assert_eq!(buckets, vec!["listed", "direct-only"]);
        });
    }

    #[test]
    fn csv_and_tsv_preview_as_tables_not_as_text() {
        // Shown as raw text a CSV is readable and not usable: the columns only
        // line up by accident.
        assert!(matches!(
            preview_kind("data.csv", None),
            PreviewKind::Table(',')
        ));
        assert!(matches!(
            preview_kind("data.tsv", None),
            PreviewKind::Table('\t')
        ));
        assert!(matches!(
            preview_kind("data.bin", Some("text/csv")),
            PreviewKind::Table(',')
        ));
        // Still text, and must stay text.
        assert!(matches!(preview_kind("a.json", None), PreviewKind::Text));
    }

    #[test]
    fn preview_kind_trusts_the_content_type_over_the_extension() {
        // A declared type wins: an image served as .dat is still an image.
        assert_eq!(
            preview_kind("blob.dat", Some("image/png")),
            PreviewKind::Image
        );
        assert_eq!(
            preview_kind("notes.png", Some("text/plain")),
            PreviewKind::Text
        );

        // Charset parameters must not defeat the match.
        assert_eq!(
            preview_kind("a.bin", Some("text/plain; charset=utf-8")),
            PreviewKind::Text
        );
        assert_eq!(
            preview_kind("index.bin", Some("text/html; charset=utf-8")),
            PreviewKind::None
        );
        assert_eq!(
            preview_kind("index.bin", Some("application/xhtml+xml")),
            PreviewKind::None
        );
        assert!(html_opens_externally("index.HTML", None));
        assert!(html_opens_externally(
            "index.bin",
            Some("text/html; charset=utf-8")
        ));
        assert_eq!(
            preview_kind("clip.bin", Some("video/mp4")),
            PreviewKind::None
        );

        // Structured text is worth reading inline.
        assert_eq!(
            preview_kind("a.bin", Some("application/json")),
            PreviewKind::Text
        );

        // A specific non-text type is a real answer — the extension must not
        // override it into something we would then fail to render.
        assert_eq!(
            preview_kind("report.txt", Some("application/pdf")),
            PreviewKind::None
        );

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
        // And the reason is carried, not flattened: "not UTF-8" and "not a
        // kind we draw" are different facts, and only one of them means the
        // file is fine and this app is the wrong tool.
        let preview = build_preview(PreviewKind::Text, "a.txt", vec![0xff, 0xfe, 0x00]);
        match preview {
            Preview::Handoff(reason) => assert!(reason.contains("UTF-8"), "{reason}"),
            _ => panic!("undecodable bytes must not render as text"),
        }

        let preview = build_preview(PreviewKind::Text, "a.txt", "xin chào".as_bytes().to_vec());
        match preview {
            Preview::Text(text) => assert_eq!(text.to_string(), "xin chào"),
            _ => panic!("valid UTF-8 should preview as text"),
        }

        // An image whose extension gives no format cannot be decoded.
        assert!(matches!(
            build_preview(PreviewKind::Image, "mystery", vec![1, 2, 3]),
            Preview::Handoff(_)
        ));
    }

    #[test]
    fn tags_split_on_the_first_equals_only() {
        assert_eq!(parse_tag("env=prod"), Some(("env".into(), "prod".into())));

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
        assert_eq!(
            delete_title(&[entry("a/report.txt", false, 0)]),
            "Xoá report.txt?"
        );
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
        assert_eq!(renamed_key("a/b/old/", "new").as_deref(), Some("a/b/new/"));
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

    fn job(id: i64, key: &str, state: JobState) -> Job {
        sized_job(id, key, state, 0, 0)
    }

    fn sized_job(id: i64, key: &str, state: JobState, transferred: u64, size: u64) -> Job {
        Job {
            id,
            direction: transfer::Direction::Upload,
            local: PathBuf::from(key),
            bucket: "demo".into(),
            key: key.into(),
            size,
            transferred,
            state,
            error: None,
            created_at: 0,
        }
    }

    /// The drawer, one line per string: `#` a heading, `[n]` a folder row and
    /// its file count, two spaces a file drawn under an open folder.
    fn drawn(rows: &[QueueRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                QueueRow::Heading(section) => format!("# {}", section.label()),
                QueueRow::Folder(folder) => format!("[{}] {}", folder.count, folder.name),
                QueueRow::Job { job, child } => {
                    format!("{}{}", if *child { "  " } else { "" }, job.key)
                }
            })
            .collect()
    }

    #[test]
    fn a_folder_of_files_is_one_row_until_it_is_opened() {
        let jobs = [
            job(1, "photos/a.jpg", JobState::Running),
            job(2, "photos/b.jpg", JobState::Running),
            job(3, "photos/c.jpg", JobState::Running),
        ];

        // Closed: three files, one line. Dropping a folder of two hundred is
        // what this is for — the flat list buried everything else in the queue.
        let closed = HashSet::new();
        assert_eq!(
            drawn(&queue_rows(&jobs, &closed)),
            ["# ĐANG CHẠY", "[3] photos"]
        );

        let open = HashSet::from([folder_group_key(&jobs[0]).unwrap()]);
        assert_eq!(
            drawn(&queue_rows(&jobs, &open)),
            [
                "# ĐANG CHẠY",
                "[3] photos",
                "  photos/a.jpg",
                "  photos/b.jpg",
                "  photos/c.jpg",
            ]
        );
    }

    #[test]
    fn a_lone_file_is_never_hidden_behind_a_folder_row() {
        let jobs = [
            job(1, "photos/only.jpg", JobState::Running),
            // At the bucket root there is no folder to collapse into, however
            // many of them there are.
            job(2, "top.txt", JobState::Running),
            job(3, "other.txt", JobState::Running),
        ];

        assert_eq!(
            drawn(&queue_rows(&jobs, &HashSet::new())),
            ["# ĐANG CHẠY", "photos/only.jpg", "top.txt", "other.txt"]
        );
    }

    #[test]
    fn a_folder_row_reports_the_bytes_of_every_file_under_it() {
        let jobs = [
            sized_job(1, "photos/a.jpg", JobState::Done, 100, 100),
            sized_job(2, "photos/b.jpg", JobState::Running, 50, 300),
        ];

        let rows = queue_rows(&jobs, &HashSet::new());
        let QueueRow::Folder(folder) = &rows[1] else {
            panic!("expected a folder row");
        };
        assert_eq!((folder.transferred, folder.size), (150, 400));
        assert!((folder.fraction() - 0.375).abs() < f32::EPSILON);
        // Both files, not only the ones still moving: the row's buttons act on
        // everything it stands for.
        assert_eq!(folder.ids, [1, 2]);
    }

    #[test]
    fn a_folder_leaves_the_running_section_only_once_every_file_has_landed() {
        let mut jobs = [
            job(1, "photos/a.jpg", JobState::Done),
            job(2, "photos/b.jpg", JobState::Queued),
        ];
        assert_eq!(folder_section(&jobs), QueueSection::Active);

        // A single broken file keeps the whole folder out of "xong", because a
        // folder filed under "xong" is one nobody goes back to look at.
        jobs[1].state = JobState::Failed;
        assert_eq!(folder_section(&jobs), QueueSection::Failed);

        jobs[1].state = JobState::Canceled;
        assert_eq!(folder_section(&jobs), QueueSection::Canceled);

        jobs[1].state = JobState::Done;
        assert_eq!(folder_section(&jobs), QueueSection::Done);
    }

    #[test]
    fn headings_keep_their_order_and_an_empty_section_draws_nothing() {
        // Root-level keys, so nothing here collapses into a folder and the
        // sectioning is the only thing under test.
        let jobs = [
            job(1, "canceled.txt", JobState::Canceled),
            job(2, "done.txt", JobState::Done),
            job(3, "failed.txt", JobState::Failed),
            job(4, "running.txt", JobState::Running),
            job(5, "paused.txt", JobState::Paused),
        ];

        // Headings come in their fixed order however the queue was filled, and
        // running and paused share one.
        assert_eq!(
            drawn(&queue_rows(&jobs, &HashSet::new())),
            [
                "# ĐANG CHẠY",
                "running.txt",
                "paused.txt",
                "# XONG",
                "done.txt",
                "# HỎNG",
                "failed.txt",
                "# ĐÃ HUỶ",
                "canceled.txt",
            ]
        );

        // A section with nothing in it draws no heading at all.
        let only_done = [job(1, "done.txt", JobState::Done)];
        assert_eq!(
            drawn(&queue_rows(&only_done, &HashSet::new())),
            ["# XONG", "done.txt"]
        );
        assert!(queue_rows(&[], &HashSet::new()).is_empty());
    }

    #[test]
    fn the_same_prefix_in_two_places_is_two_folder_rows() {
        let mut jobs = vec![
            job(1, "photos/a.jpg", JobState::Running),
            job(2, "photos/b.jpg", JobState::Running),
            job(3, "photos/c.jpg", JobState::Running),
            job(4, "photos/d.jpg", JobState::Running),
        ];
        jobs[2].bucket = "backup".into();
        jobs[3].bucket = "backup".into();
        // Two buckets, so two rows: one bar over bytes going to two different
        // places would describe neither. And the rows have to be *told apart* —
        // two lines with the same name, count and bar, differing only in a
        // bucket the drawer never shows, leaves the reader guessing which one
        // to pause.
        let mut names = drawn(&queue_rows(&jobs, &HashSet::new()));
        names.sort();
        assert_eq!(
            names,
            ["# ĐANG CHẠY", "[2] backup/photos ↑", "[2] demo/photos ↑"]
        );

        // Same bucket, opposite directions: the bucket cannot separate them, so
        // the arrow does.
        jobs[2].bucket = "demo".into();
        jobs[3].bucket = "demo".into();
        jobs[2].direction = transfer::Direction::Download;
        jobs[3].direction = transfer::Direction::Download;
        let mut names = drawn(&queue_rows(&jobs, &HashSet::new()));
        names.sort();
        assert_eq!(
            names,
            ["# ĐANG CHẠY", "[2] demo/photos ↑", "[2] demo/photos ↓"]
        );

        // And with nothing to collide with, the row stays short.
        let alone = [
            job(1, "photos/a.jpg", JobState::Running),
            job(2, "photos/b.jpg", JobState::Running),
        ];
        assert_eq!(
            drawn(&queue_rows(&alone, &HashSet::new())),
            ["# ĐANG CHẠY", "[2] photos"]
        );
    }

    #[test]
    fn a_folder_row_says_the_same_thing_as_the_files_under_it() {
        // The heading and the state cell answer different questions. A folder
        // of files that have not started files under ĐANG CHẠY — it is work
        // still to do — but the cell has to say "chờ", because that is what
        // every row underneath says. Borrowing the heading's word put two
        // answers in one column in one frame.
        let queued = [
            job(1, "photos/a.jpg", JobState::Queued),
            job(2, "photos/b.jpg", JobState::Queued),
        ];
        assert_eq!(folder_section(&queued), QueueSection::Active);
        assert_eq!(folder_state_label(&queued), "chờ");

        // One file moving is enough to call the folder moving.
        let mixed = [
            job(1, "photos/a.jpg", JobState::Queued),
            job(2, "photos/b.jpg", JobState::Running),
        ];
        assert_eq!(folder_state_label(&mixed), "đang chạy");

        // Nothing moving and something broken: the cell says "lỗi", the word a
        // failed job's own row uses — not "hỏng", which is the heading's.
        let broken = [
            job(1, "photos/a.jpg", JobState::Done),
            job(2, "photos/b.jpg", JobState::Failed),
        ];
        assert_eq!(folder_section(&broken), QueueSection::Failed);
        assert_eq!(folder_state_label(&broken), "lỗi");

        // "xong" only when the whole folder arrived.
        let done = [
            job(1, "photos/a.jpg", JobState::Done),
            job(2, "photos/b.jpg", JobState::Done),
        ];
        assert_eq!(folder_state_label(&done), "xong");
    }

    #[test]
    fn a_deeper_folder_is_its_own_row() {
        // Prefix grouping cannot see that these arrived from one dropped
        // folder, so each level gets a row. Documented rather than hidden: the
        // job carries nothing that would tell one drop from two.
        let jobs = [
            job(1, "trip/a.jpg", JobState::Running),
            job(2, "trip/b.jpg", JobState::Running),
            job(3, "trip/raw/c.dng", JobState::Running),
            job(4, "trip/raw/d.dng", JobState::Running),
        ];

        assert_eq!(
            drawn(&queue_rows(&jobs, &HashSet::new())),
            ["# ĐANG CHẠY", "[2] trip", "[2] trip/raw"]
        );
    }
}
