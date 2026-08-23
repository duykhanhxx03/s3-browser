# s3browser

A desktop S3 client written in Rust on [GPUI](https://gpui.rs), for macOS,
Windows and Linux.

It talks to Amazon S3 and to S3-compatible stores — Cloudflare R2, Backblaze
B2, Wasabi, DigitalOcean Spaces, MinIO — and treats "S3-compatible" as a
spectrum rather than a checkbox: it probes what each provider and each token
can actually do, and hides the features that would only fail.

Credentials never live in a config file. Secret keys go to the operating
system's credential store (Keychain, Credential Manager, Secret Service) and
only profile metadata is stored as JSON.

> **Status: usable, unreleased.** The application works and is used against
> real providers, but there are no signed binaries, no release automation and
> no CI. See [Status and limitations](#status-and-limitations) before
> depending on it.

## Features

**Browsing.** Tabs, a clickable breadcrumb, an `s3://` path bar, list and
grid layouts, pinned and recent places, and an all-buckets page. Listings
page automatically as you scroll; sorting by anything other than name loads
the rest of the prefix first, under explicit request and item caps, and says
when it stopped short rather than presenting a partial answer as a complete
one.

**Search.** Typing filters what is loaded, for free. Pressing Enter scans the
whole bucket — a LIST request per thousand keys, so it waits to be asked —
with caps, a stop button, and a status line that distinguishes "finished",
"stopped" and "hit the cap".

**Transfers.** An upload/download queue with progress, pause, resume and
cancel, journalled in SQLite so it survives a restart. Multipart uploads are
implemented directly rather than through the AWS transfer manager, so a
resumed job asks the server which parts it already holds instead of starting
over. Downloads use concurrent ranged GETs pinned to an ETag, so an
interrupted transfer cannot splice two versions of an object together.
Bandwidth throttling and adaptive retry for 503 SlowDown are built in.

**Object operations.** Rename, copy and move (server-side, switching to
UploadPartCopy above 5 GiB), delete with a per-object fallback for stores
without `DeleteObjects`, versioning, tags, storage class and Glacier restore,
header editing in bulk, an ACL editor, presigned URLs, and copying `s3://`
paths or bare keys.

**Preview.** Images, text — editable and saved back — and CSV/TSV rendered as
a table by an RFC 4180 parser. Everything else is handed to the operating
system rather than half-rendered in the app.

**Integrity.** CRC32 checksums are sent on upload and verified on download.
The ETag is deliberately not used for this: for a multipart object it is a
digest of digests, so comparing it against a whole-file hash fails on exactly
the large files most worth checking.

**Interface.** A command palette (`⌘K`) whose search ignores Vietnamese
diacritics, an error log that keeps the provider's own wording alongside a
plain-language summary, per-region loading and empty states, light and dark
themes following the system, and crash reports written to disk.

## Status and limitations

| | |
|---|---|
| Signed builds | None. macOS Gatekeeper rejects the unsigned `.app`; Developer ID signing and notarisation need a paid Apple Developer account. See [docs/PACKAGING.md](docs/PACKAGING.md). |
| CI and releases | None yet. Every build here is a local build. |
| Windows | Cross-compiled from macOS and verified only by inspecting the executable — never run on Windows. |
| Linux | Compiles, but has not been exercised on a Linux desktop. Window blur is unavailable outside KDE, so the theme falls back to solid. |
| AWS SSO | The device flow talks to real AWS endpoints and cannot be simulated with MinIO. Only its pure logic is unit tested; treat it as unverified. |
| ACL writes | Exercised against MinIO, which refuses `PutObjectAcl`. The write path has not run against an ACL-enabled AWS bucket. |
| Auto-update | Not implemented. |

Verification status for each area is recorded in more detail in
[PLAN.md](PLAN.md).

## Building from source

Rust stable (developed against 1.97.1) plus a per-platform toolchain:

| Platform | Additional requirements |
|---|---|
| macOS | Xcode and the **Metal Toolchain**. GPUI compiles Metal shaders at build time, and Xcode 26 ships that component separately — without it `cargo build` fails with `cannot execute tool 'metal'`. Install once with `xcodebuild -downloadComponent MetalToolchain` (688 MB). |
| Windows | MSVC build tools (Visual Studio Build Tools). GPUI uses Direct3D 11 and DirectWrite. |
| Linux | Wayland and X11 development libraries, plus a Vulkan loader — GPUI renders through blade-graphics there. |

```bash
git clone <repository-url>
cd s3browser
cargo build --release -p s3browser
```

The binary lands in `target/release/s3browser`.

Cross-compiling the Windows executable from macOS is possible and documented
in [docs/PACKAGING.md](docs/PACKAGING.md), along with the three `vendor/gpui`
patches it requires and why `-C target-feature=+crt-static` is not optional.

Packaging a macOS `.app` and `.dmg`, and everything known about signing and
notarisation, is in the same document.

## Getting started

On first run, with no profile configured, the welcome screen offers three
ways in: enter credentials manually, connect to a local MinIO, or sign in
with AWS SSO.

The credential form has presets for AWS, Cloudflare R2, Backblaze B2, Wasabi,
DigitalOcean Spaces, MinIO and a generic S3-compatible option; choosing one
fills in the endpoint and region. There is a **Test connection** button that
saves nothing, and a reveal button for the secret key.

A denied `ListBuckets` is not treated as a failed connection: a token scoped
to a single bucket — Cloudflare's recommended setup for R2 — signs correctly,
and you can reach the bucket by name or by typing an `s3://` path.

### Trying it against MinIO

```bash
scripts/minio-dev.sh start --large           # local MinIO plus sample data (needs Docker)
cargo run -p vault --example dev_profile     # create a "MinIO local" profile
S3BROWSER_DEV_SECRET=minioadmin cargo run -p s3browser
```

`S3BROWSER_DEV_SECRET` exists so that iterating on the UI does not mean
retyping a keychain password on every launch: the macOS Keychain grants
access by code signature, and an unsigned debug build gets a new signature
from every `cargo build`. It **only works in debug builds** — a release build
that accepted keys from the environment would let anyone who can set a
variable on the process choose the key it signs with, defeating the key store
entirely.

## Usage

### Keyboard shortcuts

`⌘` on macOS, `Ctrl` elsewhere.

| Key | Action |
|---|---|
| `⌘K` | Command palette — every command, including those without a button |
| `⌘F` | Focus the filter box (`Esc` clears it and returns the keyboard to the list) |
| `Enter` in the filter | Scan the whole bucket for that string; stoppable |
| `⌘L` | Type an `s3://bucket/prefix/` path directly |
| `⌘T` / `⌘W` | New tab / close tab |
| `⌘1`…`⌘9` | Switch tabs (`⌘9` is the last tab) |
| `⌘N` / `⌘⇧N` | New folder / new bucket |
| `⌘R` | Reload the current prefix |
| `⌘↑` / `⌫` | Go up one level |
| `↑` `↓` | Move the cursor |
| `⇧↑` `⇧↓` | Extend the selection |
| `⌘A` | Select all (visible rows only) |
| `⌘C` / `⌘X` / `⌘V` | Copy / cut / paste objects |
| `⌘I` | Details panel |
| `Space` | Preview |
| `⌘D` | Download the selection |
| `⌘⏎` | Rename |
| `⌘⌫` | Delete the selection |
| `⌘J` | Transfer queue |
| `Enter` | Open: folders navigate, files preview |
| Double click | Enter a folder, or preview a file |
| `⌘`+click | Add to the selection |
| `⇧`+click | Extend the selection |

### Command-line flags

| Flag | Effect |
|---|---|
| `--open bucket/prefix/` | Open a location directly at startup |
| `--verify-glass` | Ask AppKit whether the glass effect is really attached, print a report and exit (macOS) |

### Environment variables

| Variable | Effect |
|---|---|
| `S3BROWSER_DEBUG=1` | Diagnostic logging (connections, item counts, paging) |
| `S3BROWSER_GLASS=0/1` | Force solid or glass chrome, overriding the platform default |
| `S3BROWSER_CRASH_DIR=…` | Where crash reports are written |
| `S3BROWSER_DEV_SECRET=…` | Take the secret key from the environment instead of the credential store. Debug builds only |

### Files on disk

All in the platform config directory —
`~/Library/Application Support/s3browser`, `%APPDATA%\s3browser` or
`~/.config/s3browser`:

| File | Contents |
|---|---|
| `profiles.json` | Profiles, endpoints and provider quirks. No secrets |
| `settings.json` | Theme, motion, preview limit, transfer concurrency, bandwidth |
| `places.json` | Pinned and recent locations, scoped per profile |
| `crashes/` | Crash reports. The About dialog names this directory |

## Development

```
crates/
├── app/        # GPUI: window, views, theme, platform differences, crash handler
├── s3core/     # aws-sdk-s3 wrapper — no UI dependency, testable without a window
├── transfer/   # Transfer queue: multipart, resume, SQLite journal, checksums
├── vault/      # Profiles (JSON) plus secrets (OS credential store)
└── gpui_tokio/ # Vendored from Zed: bridges Tokio to GPUI's executor
vendor/gpui/    # gpui 0.2.2, vendored through [patch.crates-io]
```

`s3core`, `transfer` and `vault` deliberately know nothing about GPUI, so the
S3 logic, the transfer engine and profile handling are testable with a plain
`cargo test`.

```bash
cargo test                       # MinIO integration tests skip themselves if no server is running
scripts/minio-dev.sh start       # start one, so they do not skip
```

The MinIO tests distinguish three cases — no server, a server without seeded
fixtures, and a genuine failure — and skip rather than fail for the first
two, naming the command to run.

Every platform difference lives in
[crates/app/src/platform.rs](crates/app/src/platform.rs). Platform branches
use the `cfg!(target_os = …)` macro rather than `#[cfg]`, so every branch is
compiled and type-checked on every machine and the Windows and Linux paths
cannot rot silently while work happens on a Mac. `glass_check.rs` is the one
genuinely macOS-only module.

## Contributing

Issues and pull requests are welcome.

Two conventions worth knowing before opening a PR:

- **Commit messages are in English** and explain *why* a change is made
  rather than restating the diff. Where a change was verified against a real
  provider, the message says so; where it was not, the message says that too.
- **Do not claim more verification than was performed.** "Compiles" and
  "works" are different statements, and this project keeps them apart on
  purpose.

`PLAN.md` and the documents under `docs/` are written in Vietnamese and
record the design reasoning behind most decisions.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).

The bundled Inter font is licensed under the SIL Open Font License; see
[assets/fonts/Inter-LICENSE.txt](assets/fonts/Inter-LICENSE.txt). The
vendored copy of GPUI in `vendor/gpui/` is Apache-2.0, copyright Zed
Industries.
