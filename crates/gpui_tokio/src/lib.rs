//! Bridge between GPUI's executors and a Tokio runtime.
//!
//! Vendored from zed-industries/zed `crates/gpui_tokio` (Apache-2.0). Upstream is
//! not published to crates.io and targets zed's git checkout of gpui, so two
//! things are adapted for the released gpui 0.2.2: `gpui_util::defer` is inlined,
//! and spawning goes through `background_executor()` since `App::background_spawn`
//! does not exist yet in 0.2.2. Taking `&App` instead of a generic `AppContext`
//! also sidesteps 0.2.2's `AppContext::Result` associated type; `&mut Context<T>`
//! coerces to `&App` at the call site.
//!
//! The AWS SDK requires a Tokio reactor, while GPUI drives its own executors
//! (GCD-backed on macOS), so every SDK call goes through [`Tokio::spawn`].

use std::future::Future;

use gpui::{App, Global, Task};

pub use tokio::task::JoinError;

/// Runs the closure when the returned value is dropped. Inlined from `gpui_util::defer`.
struct Deferred<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for Deferred<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f()
        }
    }
}

fn defer<F: FnOnce()>(f: F) -> Deferred<F> {
    Deferred(Some(f))
}

/// Initializes the Tokio wrapper using a new Tokio runtime with 2 worker threads.
///
/// If you need more threads (or access to the runtime outside of GPUI), create the
/// runtime yourself and pass a handle to [`init_from_handle`].
pub fn init(cx: &mut App) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        // Since we now have two executors, let's try to keep our footprint small
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Failed to initialize Tokio");

    let handle = runtime.handle().clone();
    cx.set_global(GlobalTokio {
        owned_runtime: Some(runtime),
        handle,
    });
}

/// Initializes the Tokio wrapper using an existing Tokio runtime handle.
pub fn init_from_handle(cx: &mut App, handle: tokio::runtime::Handle) {
    cx.set_global(GlobalTokio {
        owned_runtime: None,
        handle,
    });
}

struct GlobalTokio {
    owned_runtime: Option<tokio::runtime::Runtime>,
    handle: tokio::runtime::Handle,
}

impl Global for GlobalTokio {}

impl Drop for GlobalTokio {
    fn drop(&mut self) {
        if let Some(runtime) = self.owned_runtime.take() {
            runtime.shutdown_background();
        }
    }
}

pub struct Tokio {}

impl Tokio {
    /// Spawns the given future on Tokio's thread pool, and returns it via a GPUI task.
    /// The Tokio task is cancelled if the GPUI task is dropped.
    pub fn spawn<Fut, R>(cx: &App, f: Fut) -> Task<Result<R, JoinError>>
    where
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let join_handle = cx.global::<GlobalTokio>().handle.spawn(f);
        let abort_handle = join_handle.abort_handle();
        let cancel = defer(move || {
            abort_handle.abort();
        });
        cx.background_executor().spawn(async move {
            let result = join_handle.await;
            drop(cancel);
            result
        })
    }

    /// Like [`Tokio::spawn`], but flattens a fallible future into `anyhow::Result`.
    pub fn spawn_result<Fut, R>(cx: &App, f: Fut) -> Task<anyhow::Result<R>>
    where
        Fut: Future<Output = anyhow::Result<R>> + Send + 'static,
        R: Send + 'static,
    {
        let join_handle = cx.global::<GlobalTokio>().handle.spawn(f);
        let abort_handle = join_handle.abort_handle();
        let cancel = defer(move || {
            abort_handle.abort();
        });
        cx.background_executor().spawn(async move {
            let result = join_handle.await?;
            drop(cancel);
            result
        })
    }

    pub fn handle(cx: &App) -> tokio::runtime::Handle {
        cx.global::<GlobalTokio>().handle.clone()
    }
}
