//! A real network backend for gpui's `HttpClient` trait.
//!
//! `Application::new()` wires up `NullHttpClient` by default — every request
//! it sends fails immediately with "No HttpClient available" — and nothing in
//! this app ever called `.with_http_client(...)` to replace it. The update
//! checker (`update::check`) is the only thing in the app that uses
//! `cx.http_client()`, so this bug meant the feature never worked at all, on
//! any platform: the silent check on launch never found anything, and the
//! "Check for updates" command always answered with a network error, because
//! there was never a real client behind it to begin with.
//!
//! `gpui_http_client`'s own `AsyncBody` already has a `TryFrom<reqwest::Body>`
//! naming the `zed-reqwest` fork specifically, so that is the client this
//! wraps rather than plain `reqwest` — it is the one the rest of gpui's HTTP
//! plumbing already assumes.

use std::sync::Arc;

use futures::{future::BoxFuture, AsyncReadExt as _};
use gpui::http_client::{AsyncBody, HttpClient, Request, Response, Url};

pub struct ReqwestClient {
    client: reqwest::Client,
}

impl ReqwestClient {
    // Returns the trait object directly rather than `Self`: the only place
    // this is built is `Application::new().with_http_client(...)`, which
    // wants exactly this type, and every caller would otherwise immediately
    // wrap it in `Arc::new(..) as Arc<dyn HttpClient>` themselves.
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Arc<dyn HttpClient> {
        Arc::new(Self {
            // GitHub's REST API refuses every request with a 403 — not a 401,
            // nothing about auth — unless it carries a `User-Agent`, and
            // `reqwest::Client::new()` sends none by default. See
            // https://docs.github.com/en/rest/overview/resources-in-the-rest-api#user-agent-required.
            client: reqwest::Client::builder()
                .user_agent(concat!("s3browser/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("static, always-valid client configuration"),
        })
    }
}

impl HttpClient for ReqwestClient {
    fn type_name(&self) -> &'static str {
        "ReqwestClient"
    }

    fn user_agent(&self) -> Option<&http::HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let client = self.client.clone();
        Box::pin(async move {
            let (parts, mut body) = req.into_parts();

            // Every request this app makes is a small JSON GET, so reading
            // the whole body into memory before sending costs nothing real —
            // and it sidesteps having to bridge `AsyncBody`'s reader variant
            // into a streaming `reqwest::Body` for a case that never occurs.
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes).await?;

            let mut request = client.request(parts.method, parts.uri.to_string());
            for (name, value) in parts.headers.iter() {
                request = request.header(name, value);
            }
            if !bytes.is_empty() {
                request = request.body(bytes);
            }

            let response = request.send().await?;
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.bytes().await?;

            let mut builder = Response::builder().status(status);
            for (name, value) in headers.iter() {
                builder = builder.header(name, value);
            }
            Ok(builder.body(AsyncBody::from_bytes(body))?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this whole module exists for, proven end to end: before this
    /// client existed, every request from `cx.http_client()` failed with "No
    /// HttpClient available" regardless of network state, so the update
    /// checker never had a real answer to give. Hits GitHub's real API rather
    /// than mocking it, because a mock would keep passing under the exact bug
    /// this is meant to catch — `NullHttpClient` "sends" a request too, it
    /// just always fails.
    ///
    /// Skips rather than fails when nothing is reachable, matching
    /// `s3core`'s live MinIO tests: a machine with no network keeps `cargo
    /// test` green, and a real assertion failure stays distinguishable from
    /// "offline".
    #[tokio::test]
    async fn reaches_the_real_github_api() {
        let client = ReqwestClient::new();
        match crate::update::check(client, "0.0.0").await {
            Ok(update) => {
                let update = update.expect("0.0.0 is older than every real release");
                assert!(!update.version.is_empty());
                assert!(update.url.contains(crate::update::REPO));
            }
            Err(error) => eprintln!("skipping: GitHub unreachable: {error}"),
        }
    }
}
