# S3Browser — Kế hoạch xây dựng

> Desktop S3 client chất lượng thương mại, viết bằng Rust + GPUI, UI kiểu glass (iOS/macOS), macOS-first.
> Nghiên cứu khả thi thực hiện ngày 2026-08-18, đối chiếu nguồn chính thức (crates.io, source Zed, docs AWS, docs các app thương mại).

---

## 1. Kết luận khả thi: GPUI làm được, KHÔNG cần đổi sang Tauri

| Yêu cầu | GPUI đáp ứng? | Chi tiết |
|---|---|---|
| Glass UI kiểu iOS | ✅ (kiểu frosted/vibrancy) | `WindowBackgroundAppearance::Blurred` dùng **NSVisualEffectView thật** (public API, macOS 12+) — chính là hiệu ứng blur của Zed. Không phải shader giả. |
| Liquid Glass macOS 26 (NSGlassEffectView) | ❌ | GPUI chưa tích hợp, và không có backdrop-filter trong app (không làm được sidebar kính làm mờ nội dung phía dưới). |
| Drag & drop file từ Finder vào | ✅ first-class | `ExternalPaths` + `.on_drop::<ExternalPaths>()` / `.drag_over()` — có sẵn trong bản crates.io `gpui 0.2.2`, Zed dùng hàng ngày. |
| Drag file từ app ra Finder | ⚠️ một phần | Chỉ có trên git main (`external_drag_payload`), và chỉ nhận **đường dẫn file có sẵn trên đĩa** (không có NSFilePromiseProvider). Để v2. |
| Virtualized list cho bucket lớn | ✅ | `gpui::uniform_list` render đúng số hàng hiển thị, Metal-fast — chính là file panel của Zed. |
| AWS SDK (cần Tokio) | ✅ | Vendor `gpui_tokio` (~100 dòng, trong repo Zed): `Tokio::spawn(cx, fut)` bridge Tokio ↔ GPUI executor. |
| Ecosystem | ✅ | `gpui-component 0.5.1` (13.1k sao, đang phát triển tích cực): 60+ component gồm Table/List ảo hóa, input, modal, theme, Dock layout. |

**Điều kiện chuyển sang Tauri (ghi lại để quyết định sau M0):** chỉ chuyển nếu bắt buộc phải có *layered Liquid Glass thật* — sidebar kính làm mờ chính nội dung app bên dưới, hiệu ứng lensing/tint của NSGlassEffectView. Khi đó Tauri v2 + `window-vibrancy 0.8` (`apply_liquid_glass`, public API macOS 26) + CSS `backdrop-filter` làm được. Đổi lại phải chấp nhận: list lớn chậm hơn (DOM + IPC serialize), mô hình drag-drop rắc rối (`dragDropEnabled` xung đột HTML5 DnD), mất cảm giác native. Với app duyệt file cần list hàng trăm nghìn object, **GPUI là lựa chọn ít rủi ro hơn**.

Lưu ý thêm: Liquid Glass của Apple còn bug (macOS 26.2 cache backdrop khi cửa sổ di chuyển — Apple forum #810314), nên "frosted glass kiểu Zed" hiện là posture thực dụng nhất.

### Chiến lược pin phiên bản

- **Bắt đầu**: `gpui = "0.2.2"` + `gpui-component = "0.5.1"` (tương thích nhau trên crates.io) + vendor `gpui_tokio` vào workspace.
- **Chỉ chuyển sang git main của zed** khi cần drag-out/Mica; lúc đó gpui-component cũng phải lấy từ git ở rev tương thích. GPUI là pre-1.0, có breaking change giữa các bản — mỗi lần nâng cấp là một PR riêng, không nâng "tiện thể".

---

## 2. Phạm vi tính năng (đối chiếu Transmit 5, Cyberduck, CS Browser, MSP360, ForkLift…)

### Tier 1 — Table stakes (bắt buộc, MVP)
- Quản lý nhiều connection profile; credentials lưu **macOS Keychain** (crate `keyring` 4.x).
- Hỗ trợ AWS + S3-compatible: **MinIO, Cloudflare R2, Backblaze B2, Wasabi, DigitalOcean Spaces** (custom endpoint, path-style, region auto-detect).
- Duyệt bucket/object: folder ảo theo prefix/delimiter, breadcrumb, sort, filter theo prefix, phân trang 1000 key/lần nạp dần vào list ảo hóa.
- Upload/download/copy/move/rename/delete (batch), tạo folder, drag & drop từ Finder vào (cả thư mục, đệ quy).
- Transfer queue: pause/resume/cancel, retry, progress + tốc độ, giới hạn số luồng song song.
- Multipart tự động cho file lớn (ngưỡng ~16 MiB), upload part song song.
- Public/anonymous bucket (`no_credentials`).

### Tier 2 — Professional (điểm phân biệt "S3 admin tool" với "file manager")
- **Presigned URL** với UI chọn thời hạn (SigV4 tối đa 7 ngày — *chỉ với IAM key dài hạn; STS/SSO hết hạn theo session token*, UI phải cảnh báo theo loại credential).
- Xem/sửa metadata + HTTP headers (Content-Type, Cache-Control…), tags.
- Storage class per object (Standard/IA/Intelligent-Tiering/Glacier/Deep Archive) + **Glacier restore** (chọn tier + số ngày, theo dõi trạng thái restore).
- **Versioning**: bật/tắt, liệt kê version, restore, phân biệt delete marker vs xóa vĩnh viễn version.
- SSE-S3 / SSE-KMS (chọn key), ACL viewer/editor (nhận biết bucket-owner-enforced).
- Auth nâng cao: import `~/.aws/credentials` + profiles, STS AssumeRole (+MFA), AWS SSO device flow.
- Dọn **multipart upload mồ côi** (ListMultipartUploads + abort + gợi ý lifecycle rule) — điểm người dùng phàn nàn nhiều nhất về app khác (bị tính tiền ngầm).
- Sync folder một/hai chiều (dựa `notify` 8.x FSEvents) — có thể lùi sang sau 1.0.

### Tier 3 — Differentiators (không app macOS nào có đủ; đây là khoảng trống thị trường)
- Bucket policy / CORS / lifecycle editor (hiện chỉ CS Browser trên Windows có).
- Cross-bucket / cross-account server-side copy; batch rename; Object Lock; Requester Pays.
- Drag OUT ra Finder (pre-download vào temp khi bắt đầu kéo, hoặc chỉ cho kéo item đã cache; file-promise cần contribute upstream cho GPUI).
- CLI companion, cloud-to-cloud transfer.

**Định vị thị trường**: giá phổ biến $20–60 one-time (Transmit $45, CS Browser Pro $29.95, ForkLift $19.95). Khoảng trống: *độ trau chuốt của Transmit + độ sâu admin của CS Browser, trên macOS* — chưa ai chiếm.

---

## 3. UX/UI concept

**Nguyên tắc**: một cửa sổ, giống Finder, không dạy lại người dùng. Glass là chất liệu, không phải trang trí.

- **Cửa sổ**: `window_background: Blurred`, `titlebar: appears_transparent + traffic_light_position` tùy chỉnh; mọi surface dùng màu có alpha (blur chỉ lộ qua pixel trong suốt).
- **Layout**: sidebar trái (profiles + buckets + pinned prefixes) → main pane (list/grid ảo hóa, breadcrumb trên cùng, search/filter) → inspector phải (đóng/mở, hiện metadata, permissions, preview ảnh qua `img()` native của GPUI).
- **Transfer drawer** đáy cửa sổ: thu gọn thành progress pill, mở ra thành queue đầy đủ.
- **Drag & drop**: kéo file/folder từ Finder vào → overlay highlight vùng thả (`drag_over` đổi style), thả là enqueue upload vào prefix đang mở; thả vào folder con trong list để upload vào đúng prefix đó.
- **Phím tắt chuẩn macOS**: ⌘C/⌘V copy-paste object, ⌘⌫ xóa, Space preview (Quick Look-style), ⌘F filter, ⌘K command palette (nhảy bucket/prefix, hành động).
- **Trạng thái rõ ràng**: object Glacier hiện badge + hành động restore inline; bucket versioned hiện toggle "hiện versions"; lỗi permission nói thẳng thiếu quyền gì.
- Light/dark theme theo hệ thống, tôn trọng reduced-transparency (fallback sang nền đục).

---

## 4. Kiến trúc kỹ thuật

```
s3browser/                    (Cargo workspace)
├── crates/
│   ├── app/                  # GPUI app: windows, views, theme, actions, keymap
│   ├── s3core/               # Bọc aws-sdk-s3: ProfileStore, ClientPool (per bucket-region),
│   │                         #   ObjectStore trait (list/get/put/copy/delete/presign/…)
│   ├── transfer/             # Transfer engine: queue, multipart, resume, throttle
│   ├── vault/                # keyring 4.x (macOS Keychain) — lưu access/secret key
│   └── gpui_tokio/           # vendor từ zed (Apache-2.0, ~100 dòng)
├── assets/                   # icons, themes
└── PLAN.md
```

**Luồng async**: UI (GPUI foreground) → gọi `Tokio::spawn(cx, …)` → aws-sdk-s3 chạy trên Tokio runtime 2 worker → kết quả về qua `Task<T>`, update `Entity<AppState>` → `cx.notify()` re-render. Transfer engine giữ state trong SQLite (`rusqlite 0.40`, feature `bundled`): journal transfer, upload_id để resume, cache listing.

**Cấu hình client per-profile** (các cạm bẫy S3-compatible đã xác minh):

```rust
let cfg = aws_sdk_s3::config::Builder::from(&sdk_config)
    .endpoint_url(profile.endpoint)          // MinIO/R2/B2…
    .force_path_style(profile.path_style)    // MinIO: true; AWS: false
    // Bắt buộc cho non-AWS: SDK v1.69+ mặc định gửi CRC32/CRC64 checksum
    // headers làm R2/B2 reject upload → per-profile toggle, non-AWS mặc định:
    .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
    .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
    .build();
```

- R2: region `auto`, **không** streaming/chunked upload (lý do Transmit không hỗ trợ R2 — ta dùng sized PUT/multipart nên tránh được).
- Endpoint có path prefix (Supabase style) đang lỗi trong SDK (aws-sdk-rust#1387) — validate khi tạo profile, báo rõ.
- Region auto-detect: HeadBucket đọc `x-amz-bucket-region` (xử lý cả 301), cache client theo (bucket, region).

**Transfer engine (tự viết multipart — `aws-sdk-s3-transfer-manager` 0.2 còn Developer Preview, "NOT recommended for production", chưa có pause/resume):**
- Part 8–16 MiB (min 5 MiB, max 10.000 part, tự tăng part size cho file khổng lồ), 4–8 part song song qua `tokio::sync::Semaphore` + `JoinSet`.
- **Resume**: lưu `upload_id` + ETag các part đã xong vào SQLite; khởi động lại → `list_parts()` đối soát rồi tiếp tục. Pause = hủy future đang bay, giữ upload_id. Cancel = `abort_multipart_upload()`.
- **Download**: ranged GET song song + `If-Match` ETag để resume an toàn; ghi file tạm rồi rename.
- Backoff cho 503 SlowDown (S3 giới hạn ~3.500 write/5.500 read mỗi giây mỗi prefix); cap tổng concurrency + bandwidth throttle ở tầng queue.
- Integrity: **không** dùng ETag như MD5 (multipart ETag không phải MD5) — dùng `x-amz-checksum-*`/GetObjectAttributes khi khả dụng.

**Ngữ nghĩa S3 phải xử lý đúng:**
- *Rename/move không tồn tại*: CopyObject + Delete; CopyObject cap 5 GB → trên đó dùng UploadPartCopy; "đổi tên folder" = N copy + N delete có progress + khôi phục khi fail giữa chừng; giữ metadata/storage-class/SSE qua `x-amz-metadata-directive`.
- *Delete*: DeleteObjects batch 1000; bucket versioned → tạo delete marker (UI phân biệt rõ 2 mức xóa); flow "empty bucket" xóa theo trang gồm cả versions.
- *Folder* là quy ước `prefix/` (object 0 byte) — tạo/hiển thị tương thích các app khác.
- *Unicode*: APFS chuẩn hóa NFD, S3 key là byte-exact — chuẩn hóa NFC khi upload, so khớp cẩn thận khi sync (bug kinh điển).
- *Chi phí*: LIST/PUT ~$0.005/1k request — **không** HEAD từng object để lấy metadata khi listing (lỗi Cyberduck bị chê chậm + tốn tiền); metadata chỉ load khi mở inspector.

---

## 5. Roadmap

### M0 — Spike khả thi ✅ **XONG (18/08/2026) — 4/4 gate pass, GPUI đã khóa**

| Gate | Bằng chứng đo được |
|---|---|
| Glass UI | `--verify-glass` hỏi thẳng AppKit: `BlurredView` (subclass `NSVisualEffectView`) **có thật trong view hierarchy**, window non-opaque, alpha 0.0001, FullSizeContentView, titlebar trong suốt |
| List ảo hóa 100k | `built rows 0..22 of 100000` — chỉ 22 phần tử được dựng |
| Drop từ Finder | Test tự động mô phỏng `FileDropEvent`, listener chạy, state đổi |
| AWS SDK trên Tokio | Trong GUI: `connected via gpui_tokio, 2 buckets` → `listed 5 entries`; +3 integration test với MinIO |

**Ba điều chỉnh so với nghiên cứu ban đầu** (chi tiết trong README):
1. `gpui_tokio` upstream **không build được với gpui 0.2.2** — nhắm bản git của Zed (`App::background_spawn` chưa tồn tại; generic `AppContext::Result` không unify). Bản vendor đã sửa để nhận `&App` + `background_executor()`.
2. `ExternalPaths` **không dựng được từ crate ngoài** ở 0.2.2 (field `pub(crate)`) → test drop chỉ gửi payload rỗng; muốn test payload thật phải dùng git rev.
3. **Metal Toolchain (688 MB) là dependency ẩn** của GPUI — `xcodebuild -downloadComponent MetalToolchain`, phải đưa vào hướng dẫn onboarding.

### M1 — Browse (2–3 tuần)
Profile manager + Keychain + import `~/.aws`; sidebar buckets; list object ảo hóa nạp trang dần (continuation token, hủy khi đổi thư mục); breadcrumb, sort, filter; tạo/xóa bucket, folder; context menu; light/dark theme.

### M2 — Transfers (3–4 tuần)
Drag-drop upload (đệ quy thư mục) + overlay; download; transfer drawer với queue persist qua SQLite (pause/resume/cancel/retry, tốc độ, throttle); multipart engine đầy đủ như §4; xử lý 503/retry; dọn multipart mồ côi.

### M3 — Object ops + chia sẻ (2–3 tuần)
Rename/copy/move (gồm >5 GB), batch delete có confirm; presigned URL UI (cảnh báo theo loại credential); copy public URL; metadata/tags editor; storage class + Glacier restore; preview ảnh/text trong inspector, Space để preview nhanh; mở bằng app ngoài (`opener`).

### M4 — Pro (3–4 tuần)
Versioning UI đầy đủ; SSE-S3/KMS; ACL editor; STS AssumeRole + MFA; AWS SSO device flow; empty-bucket flow; command palette ⌘K; keyboard shortcuts hoàn chỉnh.

### M5 — Ship (2–3 tuần)
Onboarding lần đầu; polish theme/animation/reduced-transparency; crash reporting (sentry + minidump); đóng gói `cargo-packager` (.app + .dmg), ký Developer ID + notarize (`rcodesign` hoặc notarytool), auto-update (`cargo-packager-updater` hoặc Velopack). **Phân phối trực tiếp, không App Store** (App Sandbox siết drag-drop/bookmark; GPUI chưa có accessibility — rủi ro review).

Sau 1.0: sync hai chiều, bucket policy/CORS/lifecycle editor, drag-out, cross-account copy, Windows/Linux (GPUI đã ổn trên Windows từ 10/2025, blur = Acrylic/Mica).

---

## 6. Testing

- **Unit**: transfer engine test bằng `aws-smithy` `StaticReplayClient` (không cần server); logic list/rename/delete test với `s3s` (S3 server in-process).
- **Integration**: docker MinIO + LocalStack trong CI; matrix chạy định kỳ với AWS thật + R2 + B2 để bắt quirk checksum/addressing.
- **UI**: `#[gpui::test]` (executor deterministic của GPUI).
- **Chaos**: kill app giữa multipart → mở lại phải resume; rớt mạng giữa chừng → retry/backoff đúng.

---

## 7. Rủi ro chính

| Rủi ro | Mức | Đối sách |
|---|---|---|
| GPUI pre-1.0, breaking changes, docs mỏng | Cao | Pin 0.2.2 + gpui-component 0.5.1; đọc source Zed làm docs; nâng cấp có chủ đích |
| Không có Liquid Glass thật (chỉ frosted blur toàn cửa sổ) | Trung | Chấp nhận ở 1.0 (đẹp kiểu Zed); theo dõi zed#38400; Tauri là phương án đã nghiên cứu sẵn |
| GPUI chưa có accessibility/screen-reader | Trung-cao (sản phẩm thương mại) | Ghi nhận công khai; phím tắt đầy đủ; theo dõi upstream |
| S3-compatible quirks (checksum, path-style, region) | Trung | Per-profile toggles + preset sẵn cho MinIO/R2/B2/Wasabi/Spaces (học mô hình profile của Cyberduck) |
| Drag-out cần file promise mà GPUI không có | Thấp (ngoài scope 1.0) | v2: pre-download to temp hoặc contribute upstream |
| Chi phí request cho user (LIST/HEAD) | Trung | Không HEAD hàng loạt; cache listing; hiển thị số request khi debug |

---

## 8. Việc cần làm ngay (M0, theo thứ tự)

1. `rustup` + Xcode Command Line Tools.
2. `cargo new` workspace theo cấu trúc §4; commit đầu tiên + `git init`.
3. Spike cửa sổ glass (WindowOptions như §3) — chụp màn hình đối chiếu kỳ vọng.
4. Spike drop từ Finder + uniform_list 100k hàng.
5. `docker run minio/minio` + vendor gpui_tokio + list bucket đầu tiên hiển thị lên list.
6. Review kết quả M0 → chốt stack, bắt đầu M1.
