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

### M1 — Browse ✅ **XONG (18/08/2026)**

Profile manager + credential store OS + import `~/.aws`; sidebar profile/bucket; list ảo hóa **phân trang tự động khi cuộn**; breadcrumb bấm từng cấp; sort theo tên/kích thước/ngày; filter; tạo folder + bucket; xoá nhiều mục (đệ quy, batch 1000); light/dark theo hệ thống. 28 test pass.

**Cross-platform** (theo yêu cầu bổ sung): mọi khác biệt macOS/Windows/Linux gom vào `platform.rs` — nền cửa sổ (blur / Acrylic / solid vì Linux chỉ KDE có blur), traffic lights, font stack, phím `⌘`/`Ctrl`, credential store, thư mục config. Theme chỉ đổi **nền cửa sổ** giữa glass và solid; mọi panel là lớp alpha nên đúng ở cả hai chế độ.

**Còn nợ kỹ thuật, làm ở M2:** dùng `gpui-component 0.5.1` (đã kiểm chứng build được với gpui 0.2.2) để thay ô nhập tự chế bằng `Input`, thêm context menu chuột phải và dialog xác nhận. Lưu ý khi tích hợp: icon cần `AssetSource` với file SVG Lucide đặt tại `icons/<tên>.svg`, và `Root` phải tự render 3 layer dialog/sheet/notification.

**Ba API thiếu ở gpui 0.2.2 đã phải tự xử lý:** không có ô text input (dùng cơ chế bắt phím chung cho filter + đặt tên), không có `on_double_click` (dùng `ClickEvent.click_count`), không có accessor "visible range" công khai trên `UniformListScrollHandle` (dùng chính range mà `uniform_list` truyền vào callback).

### M2 — Transfers ✅ **XONG (19/08/2026)**
Drag-drop upload (đệ quy thư mục) + overlay; download; transfer drawer với queue persist qua SQLite (pause/resume/cancel/retry, tốc độ, throttle); multipart engine đầy đủ như §4; xử lý 503/retry; dọn multipart mồ côi. 61 test pass (55 unit + 6 integration MinIO).

**Đã kiểm chứng với MinIO thật:** upload multipart 23 MiB rồi tải về so khớp từng byte; hoàn tất một multipart dở từ lần chạy trước qua `ListParts`; cancel không để lại upload mồ côi; upload mồ côi tìm được và huỷ được.

**Ba điều đáng ghi lại:**
1. **Multipart tự viết** thay vì `aws-sdk-s3-transfer-manager` 0.2 — bản đó còn Developer Preview và chưa có pause/resume, đúng như dự đoán ở §4.
2. **Resume hỏi server, không tin sổ sách của mình**: mỗi part server nhận là ghi ngay vào journal SQLite, nhưng khi tiếp tục thì `ListParts()` mới là nguồn sự thật — tránh trường hợp crash giữa lúc ghi journal làm gửi lại part đã có.
3. **Throttle ở mức một part**, không bọc body stream của SDK: tốc độ trung bình đúng giới hạn nhưng từng part vẫn đi thành cụm. Bọc stream sẽ chính xác hơn nhưng phải can thiệp sâu vào SDK, không đáng cho một cái cap.

### M3 — Object ops + chia sẻ ✅ **XONG (19/08/2026)**
Rename/copy/move (gồm >5 GB), batch delete có confirm; presigned URL UI (cảnh báo theo loại credential); copy public URL; metadata/tags editor; storage class + Glacier restore; preview ảnh/text trong inspector, Space để preview nhanh; mở bằng app ngoài (`opener`). 80 test pass (13 integration MinIO).

**Một lỗi có sẵn lộ ra khi làm presigned URL:** phần import `~/.aws/credentials` bỏ qua `aws_session_token`, nên mọi profile sinh từ STS/SSO nhập vào đều hỏng xác thực với lỗi chữ ký khó hiểu. Đã sửa; token lưu ở keychain entry riêng để secret cũ vẫn đọc được.

**Bốn chỗ S3 không cư xử như trực giác, đều đã có test chặn:**
1. **CopyObject không giữ storage class** — mặc định đưa bản sao về STANDARD bất kể nguồn, nên copy một object Glacier là âm thầm đẩy nó sang tầng đắt hơn nhiều. Phải HEAD nguồn rồi set lại.
2. **`x-amz-copy-source` gửi nguyên văn** — key có dấu cách hay `+` mà không percent-encode thì server đọc ra key khác: sao chép nhầm object hoặc 404 khó hiểu.
3. **Xoá trong bucket versioning không xoá gì cả** — chỉ tạo delete marker, bản cũ vẫn còn và vẫn tính tiền. Nói "không hoàn tác được" ở đó là sai.
4. **`x-amz-restore` vắng mặt ở hai ca khác hẳn nhau** — chưa bao giờ lưu trữ, và đã lưu trữ mà chưa ai yêu cầu khôi phục. Chỉ storage class mới tách được, và chúng cần hai giao diện khác nhau. `GLACIER_IR` không cần khôi phục dù tên nghe giống Glacier.

**Metadata chỉ nạp khi mở inspector**, không HEAD từng dòng lúc listing — đúng như §4 đã cảnh báo, đó là lỗi khiến client S3 khác vừa chậm vừa tốn tiền.

### M4 — Pro (3–4 tuần) — **gần xong (19/08/2026)**
Versioning UI đầy đủ ✅; SSE-S3/KMS ✅; **ACL editor — chưa làm**; STS AssumeRole + MFA ✅; AWS SSO device flow ✅ (chưa kiểm chứng); empty-bucket flow ✅; command palette ⌘K ✅; keyboard shortcuts hoàn chỉnh ✅. 93 test pass (16 integration MinIO).

**Mức độ kiểm chứng — ba bậc khác nhau, cần phân biệt rõ:**

| Phần | Kiểm chứng |
|---|---|
| Versioning, empty-bucket, AssumeRole | **Chạy thật với MinIO.** Empty-bucket được chứng minh bằng cách gọi `DeleteBucket` sau khi dọn — còn sót version nào là lệnh đó fail. AssumeRole được chứng minh bằng cách kết nối lại bằng credential tạm và liệt kê bucket. |
| SSE-S3/KMS | **Một nửa.** MinIO tiêu chuẩn không có KMS backend nên từ chối mã hoá — nhưng chính lời từ chối đó chứng minh header đã gửi đi, vì server không thể phàn nàn về thứ nó chưa từng được yêu cầu. Việc object lưu xuống có thật sự được mã hoá thì chưa kiểm được. |
| AWS SSO device flow | **Chưa kiểm chứng gì.** Nó nói chuyện với endpoint AWS thật, không giả lập được bằng MinIO. Chỉ phần logic thuần (nhịp poll, hạn hết hiệu lực, nhãn role) là có unit test. Cần chạy với một Identity Center thật trước khi tin. |

**ACL editor chưa làm** — và nên cân nhắc có đáng làm không: AWS khuyến nghị tắt ACL (Object Ownership = bucket owner enforced) từ 2023, bucket mới mặc định đã tắt. Làm một editor cho cơ chế mà chính AWS khuyên đừng dùng thì giá trị thấp; bucket policy có ích hơn nhiều và đang nằm ở phần "sau 1.0".

### M5 — Ship (2–3 tuần) — **đang làm**
Xong: reduced-transparency (glass tự tắt khi bật cài đặt trợ năng), màn hình bắt đầu, cấu hình `cargo-packager` + dựng thật `.app`/`.dmg` chưa ký (xem `docs/PACKAGING.md`).

**Chặn bởi thứ không phải code:** ký Developer ID và notarize cần tài khoản Apple Developer trả phí; máy dev hiện chỉ có chứng chỉ *Apple Development*, loại chỉ chạy thử cục bộ được. Auto-update cần nơi host và một cặp khoá ký — khoá riêng không được nằm trong repo, nên phải quyết định chỗ host trước rồi mới làm.

**Ba điều đo được khi đóng gói:**
1. `cargo packager` không tự build, chạy thẳng sẽ báo lỗi trỏ vào binary chưa tồn tại.
2. Bundle vừa dựng xong có chữ ký **không hợp lệ** (`code has no resources but signature indicates they must be present`) — linker ký cho file binary đơn lẻ, không ký cho cấu trúc bundle bọc quanh nó sau đó. Phải `codesign --force --deep --sign -` lại kể cả để chạy thử.
3. Ký ad-hoc xong `codesign --verify` báo hợp lệ nhưng `spctl` **vẫn từ chối** — chữ ký hợp lệ về cấu trúc không đồng nghĩa với được phép phân phối.

**Crash reporting:** phần bắt lỗi cục bộ đã xong và có kiểm chứng bằng crash thật — panic hook ghi báo cáo (phiên bản, nền tảng, vị trí, thông điệp, backtrace) vào `~/Library/Application Support/s3browser/crashes/`. Phần gửi lên Sentry **cố tình chưa nối**: nó cần DSN thuộc về người vận hành dự án, mà code viết cho một endpoint không ai gọi được thì không kiểm chứng được — nó sẽ trông như đã xong trong khi chưa hề chạy thử.

### Chi tiết M5 gốc
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

---

## 6. Đối chiếu với yêu cầu thương mại hoá (19/08/2026)

Đối chiếu bảng tính năng trong `s3-browser-commercial-features.md` với những gì
thực sự đã build. Chỉ đánh ✅ cho thứ đã chạy được, không đánh cho thứ mới có API.

### Đã có

**P0 gần đủ.** Connection (AWS + endpoint tuỳ ý, access/secret/session token,
import `~/.aws`, path-style/virtual-host) ✅. Browse kiểu Finder, upload
file/thư mục, drag & drop, rename/move/delete ✅. Transfer engine: multipart,
song song, pause/resume/retry, resume sau khi restart, queue có tiến trình và
tốc độ ✅. Keychain ✅. Presigned URL ✅.

**P1 quá nửa.** Filter, preview ảnh/text/json, metadata + tags + storage class +
Glacier restore, versioning đầy đủ, SSE-S3/KMS, bandwidth throttling, nhận diện
provider từ endpoint (R2/B2/Wasabi/Spaces/MinIO) ✅.

**P2 một phần.** SSO device flow (chưa kiểm chứng), AssumeRole + MFA ✅.

### M6 — Vá P0 (ưu tiên cao nhất)

Bốn thứ này nằm ở P0 trong tài liệu mà chưa có. Một trong số đó là lỗi im lặng.

1. **Tải xuống bỏ qua thư mục.** `download_selection` lọc `!entry.is_folder`,
   nên chọn một thư mục rồi bấm tải xuống thì **không có gì xảy ra và không có
   thông báo nào**. Tài liệu xếp "download folder" ở P0. Đây là lỗi trước, tính
   năng sau.

2. **Checksum verification.** Chưa có. Tài liệu xếp P0 và đặt nó trong nhóm ba
   yếu tố cạnh tranh. PLAN §4 đã ghi cách làm đúng: **không** dùng ETag như MD5
   vì ETag của multipart không phải MD5 — dùng `x-amz-checksum-*` khi provider
   hỗ trợ. Việc này phụ thuộc capability detection ở M7 vì không phải provider
   nào cũng có.

3. **ETA.** Queue đang hiện tốc độ nhưng không hiện thời gian còn lại. Chính
   tài liệu lấy `16m remaining` làm ví dụ UI. Rẻ, và là thứ người dùng nhìn.

4. **Copy/duplicate trong UI.** `copy_object` đã có và đã test (kể cả đường
   multipart >5 GB), chỉ thiếu lệnh trong giao diện.

### M7 — Capability detection ✅ **XONG (19/08/2026)**

Tài liệu gọi đây là **kiến trúc lõi chứ không phải checkbox marketing**, và
phiên vừa rồi chứng minh điều đó: token R2 phạm vi một bucket bị `AccessDenied`
ở `ListBuckets` và suýt làm app vô dụng. Hiện app đoán quirk từ endpoint
(`with_provider_defaults`) nhưng không hỏi provider xem nó làm được gì.

Cần dò và cache theo (provider, bucket): versioning, object lock, tagging,
lifecycle, checksum algorithms, presigned PUT. UI tự tắt thứ không hỗ trợ thay
vì để người dùng bấm rồi ăn `501 NotImplemented`. Đã có sẵn hai ca thật để
kiểm: MinIO không có KMS, R2 không cho ListBuckets.

### M8 — Cấu hình bucket

Lifecycle rules, CORS editor, bucket policy editor, public access block. Bốn
thứ P1 chưa đụng tới. Nhóm này hợp với gói Pro, và là lý do người ta rời AWS
Console.

### M9 — Local ↔ S3 sync

Tài liệu gọi là killer feature, và mình đồng ý: nó biến sản phẩm từ "một S3
client nữa" thành công cụ workflow. Hai panel, compare, preview thay đổi trước
khi chạy, các chế độ upload-only / download-only / mirror hai chiều, exclude
pattern, bảo vệ khỏi xoá nhầm.

Cảnh báo phạm vi: đây là milestone lớn nhất trong danh sách, lớn hơn cả M2. So
sánh cây thư mục hai bên đúng cách (mtime, size, checksum, chuẩn hoá Unicode
NFC/NFD như §4 đã ghi) là phần khó, không phải phần UI.

### M10 — Enterprise

Object Lock / Legal Hold / retention, proxy support, SSE-C, audit log, cấu hình
tập trung. Làm khi có khách hàng thật yêu cầu, không làm trước.

### Cố tình không làm

- **Mount S3 như ổ đĩa.** Cần FUSE/kernel extension trên macOS, ký riêng, và là
  một sản phẩm khác chứ không phải một tính năng.
- ~~**ACL editor**~~ — đã làm theo yêu cầu (19/08/2026). Nhận định cũ vẫn giữ:
  AWS khuyến nghị tắt ACL từ 2023 và bucket mới mặc định đã tắt, nên bucket
  policy ở M8 vẫn hữu ích hơn. Bù lại nó là ca dùng tốt cho M7: panel tự ẩn ở
  bucket không bật ACL thay vì hiện ra rồi báo `AccessControlListNotSupported`.
- **Scheduled sync** cho tới khi M9 chạy ổn định — lên lịch cho một thứ chưa
  đáng tin là nhân bản lỗi lên.

### Thứ tự đề xuất

M6 trước, vì có một lỗi im lặng và ba thứ P0. M7 tiếp theo vì M6 phụ thuộc nó
cho checksum. Rồi M9 nếu mục tiêu là khác biệt hoá, hoặc M8 nếu mục tiêu là bán
được cho người đang dùng AWS Console.

---

## 7. Đợt hoàn thiện giao diện (21/08/2026)

Xong: bộ icon vẽ theo phong cách iOS glyph, font Inter đi kèm binary, bảng màu
kiểu Radix, thumbnail ảnh, clipboard copy/cắt/dán, ngày tháng theo giờ địa
phương và diễn đạt tương đối.

Năm mục còn lại, làm lần lượt:

### 7.1 Menu chuột phải — xong
Menu theo ngữ cảnh qua `Action` của gpui, nên menu và phím tắt cùng chạm tới một
handler. Mục không áp dụng được thì bỏ hẳn, trừ Dán: nó phụ thuộc clipboard chứ
không phụ thuộc dòng đang bấm, nên hiện mờ thay vì nhấp nháy giữa các lần mở.

### 7.2 Điều hướng bằng phím mũi tên — xong
Con trỏ bàn phím giữ theo *key* của object chứ không theo số dòng: lọc và sắp xếp
đánh số lại các dòng, giữ số dòng thì con trỏ tự trượt sang tệp khác mà không ai
bấm phím nào, và Enter tiếp theo mở nhầm thứ.

Cuộn theo con trỏ: `scroll_to_item` của gpui không có chiến lược "gần nhất", nên
chiều di chuyển chọn mép — xuống thì canh mép dưới, lên thì canh mép trên. Chọn
nhầm mép biến một bước một dòng thành một cú nhảy cả trang. Command palette dùng
chung cơ chế đó, nên phần nó còn thiếu cũng xong luôn.

Đã xem tận mắt trên MinIO: mũi tên đi đúng dòng, danh sách cuộn từng dòng một khi
con trỏ chạm mép, `⇧↑` chọn đúng dải liên tiếp. Phím giả lập tới được GPUI, khác
với chuột giả lập ở §5.

### 7.3 Trạng thái đang tải theo từng vùng — xong
Placeholder tĩnh theo đúng hình dạng của vùng: danh sách có hàng giả đúng ba cột,
sidebar có tên bucket giả, inspector có hàng metadata giả. Tĩnh chứ không nhấp
nháy: hiệu ứng động cần vẽ lại mỗi khung hình suốt thời gian chờ, mà app đã chạy
một vòng lặp như thế cho tiến độ truyền rồi.

Bỏ chữ "đang tải" chung ở thanh trạng thái, vì giờ mỗi vùng tự nói.

Thêm cả trạng thái *hết* của vùng: thư mục trống và bộ lọc không khớp là hai
chuyện khác nhau, trước đây trông giống hệt nhau. Cái sau có nút xoá bộ lọc.

Một lỗi lộ ra khi làm: `cx.notify()` gọi từ trong processor của `uniform_list` bị
bỏ qua, vì khung hình đang vẽ không tự vẽ lại chính nó. Nên dòng "đang tải thêm"
chỉ hiện sau khi trang nó báo đã về, tức là không bao giờ. Sửa bằng cách xin vẽ
lại từ bên ngoài khung hình, ở bước đầu của task.

Đã xem tận mắt: placeholder sidebar, placeholder danh sách, thư mục trống, bộ lọc
không khớp, và dòng "Đang tải thêm…" khi sang trang. Riêng placeholder của
inspector thì chưa thấy chạy.

### 7.4 Thông báo lỗi thao tác được — xong
Một lỗi giờ tách làm ba phần: **tóm tắt** nói bằng tiếng người, **nguyên văn**
giữ đúng lời provider vì đó mới là thứ dán vào ticket, và **nút sửa** chỉ có ở
những ca thật sự có đúng một việc để làm.

Giữ cả danh sách chứ không một ô: một lượt xoá hàng loạt lỗi theo từng key, ô đơn
thì chỉ còn đúng một lỗi trên màn hình và không có đường tới những cái còn lại.
Không tự xoá khi điều hướng — lỗi tự biến mất trong lúc đang đọc là lỗi không ai
đọc được.

Phân loại nằm ở `crates/app/src/failure.rs`, thuần và có test. Ba nhóm phải tách
bạch: khoá sai (sửa profile), không đủ quyền (không có nút, vì thiếu quyền gì thì
tuỳ request mà đoán bừa là chỉ người ta đi sửa cái đang đúng), và mạng hoặc bị
chặn tốc độ (thử lại). Không nhận ra thì giữ nguyên lời provider chứ không bịa
một câu thân thiện.

Một lỗi có sẵn lộ ra khi làm: mọi chỗ đều dùng `{error}`, mà `Display` của
`anyhow` chỉ in lớp context ngoài cùng, nên `.context("ListBuckets failed")` vứt
mất nguyên văn của provider. Đổi hết sang `{error:?}`. Trước khi đổi, một khoá
sai bị báo thành "token không có quyền liệt kê bucket".

Khu trống ở giữa cũng thôi đoán: trước nó khẳng định "token có thể chỉ có quyền
trên một bucket" cho mọi lỗi, giờ nó nói đúng cái vừa hỏng và kèm nút.

Đã xem tận mắt: khoá sai cho ra "Khoá truy cập không đúng" kèm nút Sửa profile,
bảng nhật ký mở được từ command palette, có nguyên văn cắt gọn và nút chép.

### 7.5 Tìm kiếm file và bucket — xong
Một ô, hai mức. Gõ thì lọc những gì đã tải, miễn phí và chạy ngay. Enter thì quét
cả bucket, tốn một yêu cầu LIST mỗi nghìn key — nên nó đợi người ta bấm chứ không
tự chạy.

Kết quả đổ thẳng vào `entries`, đúng chỗ một listing vẫn đổ vào, nên chọn, tải
xuống, xem chi tiết và menu chuột phải đều chạy tiếp mà không cần biết có tìm
kiếm. Tất cả đều thao tác trên `entry.key`, vốn là key thật dù đến từ đâu.

Quét từng trang một chứ không một lệnh chạy tới hết: bucket triệu key là nghìn
yêu cầu, mà một cuộc quét chỉ hiện gì đó lúc xong là cuộc quét không ai lượng
được giá, cũng không dừng được. Thanh trên danh sách nói rõ tìm gì, được bao
nhiêu, quét bao nhiêu mục trong bao nhiêu yêu cầu, và đang quét hay đã dừng hay
đã xong — vì "không tìm thấy" từ một cuộc quét mới đi được một phần mười bucket
là câu app không có cơ sở để nói.

Bộ lọc bucket ở sidebar chỉ hiện khi có từ 10 bucket trở lên; dưới đó cả danh
sách đã nằm trên màn hình rồi.

Cả hai bộ lọc giờ bỏ dấu như command palette, nên lọc lại một tập kết quả bằng
chính chuỗi đã tìm ra nó không thể giấu mất cái nào.

Đã xem tận mắt trên prefix 1200 key: quét qua ranh giới trang, ra đúng
`many/file-0999.txt` kèm đường dẫn, và ca không khớp nói rõ đã quét hết bucket.

---

## 8. Đối chiếu với CS Browser / s3browser.com (21/08/2026)

Đối chiếu bảng tính năng công bố ở s3browser.com với những gì app này đã có.
Nhiều mục trùng với §6 nên không nhắc lại; dưới đây là phần §6 chưa nói tới.

### Một lỗi lộ ra khi đối chiếu

**Upload không đặt Content-Type.** `put_object` và `create_multipart_upload` đều
không gọi `.content_type(..)`, nên mọi object tải lên bằng app này được lưu với
kiểu mặc định của provider, thường là `application/octet-stream`. Hậu quả không
thấy ngay trong app — inspector vẫn đọc được kiểu từ `HeadObject` — mà thấy ở
chỗ khác: một cái ảnh chia sẻ bằng presigned URL sẽ bị trình duyệt tải về thay vì
mở ra, và một trang tĩnh phục vụ từ bucket sẽ hỏng hoàn toàn.

CS Browser có hẳn một "HTTP Headers Editor" và bộ header mặc định cho việc này,
tức là họ coi đây là chuyện thường ngày chứ không phải ca hiếm. Đây là lỗi trước,
tính năng sau.

### Rẻ mà đáng làm ngay

1. ~~**Content-Type khi upload**~~ — xong (21/08/2026). Đoán từ đuôi file bằng
   `mime_guess`, đặt ở cả `PutObject` lẫn `CreateMultipartUpload`. Không có đuôi
   thì không đặt gì, vì bịa một kiểu còn tệ hơn mặc định của provider.
2. ~~**Sửa header của object đã có**~~ — xong (21/08/2026). Content-Type,
   Cache-Control, Content-Disposition, sửa bằng cách copy đè lên chính nó với
   `MetadataDirective=REPLACE`. Ô để trống nghĩa là xoá header đó. ACL riêng của
   object không sống sót qua một lần copy — chỉ ảnh hưởng bucket còn bật ACL,
   nhưng là mất thật.
3. ~~**Xem CSV dạng bảng**~~ — xong (21/08/2026). Cả `.csv` lẫn `.tsv`. Parser
   viết tay theo RFC 4180; cột rộng theo ô dài nhất; cắt ở 200 dòng và 24 cột,
   và nói ra phần bị cắt. Parquet vẫn cần thêm thư viện, tính sau.
4. ~~**Áp hàng loạt**~~ — xong (21/08/2026). ACL và header cho cả vùng chọn, một
   key mỗi lượt để có tiến trình và dừng được, lỗi gom lại thành một mục trong
   nhật ký chứ không phải một dòng đỏ mỗi key. Đường đơn lẻ và đường hàng loạt là
   cùng một đường, để phần báo cáo không trôi ra hai kiểu.

   Đã kiểm chứng với MinIO thật: chọn 5 object, gõ `image/png`, cả 5 đổi đúng.

### Vừa sức, nhưng chưa gấp

- **Requester Pays**: một header trên mỗi request, và một ô tick trong profile.
- **Transfer Acceleration**: đổi endpoint sang `s3-accelerate`, chỉ AWS.
- **Bucket logging** và **static website hosting**: cùng nhóm với M8 (lifecycle,
  CORS, bucket policy), nên gộp vào đó chứ không tách ra.
- **Proxy**: đã nằm ở M10.

### Lớn

- **Sync hai chiều** — đã là M9, và CS Browser cũng coi đây là mũi nhọn.
- **Nén và mã hoá phía client (AES-256) trước khi upload.** Khác với SSE: khoá
  không bao giờ rời máy. Nhưng nó tạo ra một định dạng riêng mà chỉ app này đọc
  được, nên phải cân nhắc kỹ trước khi hứa — dữ liệu mã hoá bằng một khoá người
  dùng làm mất là dữ liệu đã mất hẳn.

### Đề nghị không làm

- **Quản lý CloudFront.** Là một sản phẩm khác, không phải một tính năng.
- **TinyURL.** Gửi key của người dùng sang một dịch vụ thứ ba để đổi lấy một
  đường link ngắn hơn.
- **Command-line tools.** Đáng làm, nhưng là một binary khác và một bề mặt khác
  để bảo trì; chỉ nên bắt đầu khi phần GUI đã ổn định.

### Một lo ngại về quy mô

CS Browser quảng cáo "xử lý hàng triệu file". App này phân trang khi liệt kê,
nhưng sắp xếp và lọc thì chạy trên toàn bộ `entries` đang giữ trong bộ nhớ. Một
prefix triệu key sẽ là một triệu `Entry` trong RAM và một lần sort trên mỗi trang
mới về. Chưa đo, nên chưa biết ngưỡng thật ở đâu — nhưng đây là thứ phải đo trước
khi in con số đó lên trang bán hàng.

---

## 9. Đối chiếu với Brows3 (21/08/2026)

`github.com/rgcsekaraa/brows3` — Tauri + Next.js, Rust ở lõi, cùng hạng sản phẩm.
Đọc README và mã nguồn.

### Một lỗi lộ ra khi đối chiếu

**Sắp xếp chỉ đúng trên phần đã tải** — đã sửa (21/08/2026). `resort_and_filter` sắp `entries`, mà
`entries` chỉ là những trang đã về. Sắp theo kích thước trên một prefix 1200 key
cho ra *cái lớn nhất trong 1000 key đầu*, không phải cái lớn nhất trong prefix —
và không có gì trên màn hình nói rằng đó là câu trả lời một phần.

Tệ hơn: `load_more` nối trang mới rồi sắp lại toàn bộ, nên **các dòng đang nhìn
nhảy chỗ** trong lúc cuộn. Brows3 giải quyết bằng cách sắp trọn bộ kết quả trước
khi phân trang, có trần rõ ràng (100.000 mục hoặc 100 lượt LIST) và cache lại
theo phiên.

Đây là lỗi trước, tính năng sau — và cùng loại với lỗi Content-Type: app không sai
một cách ồn ào, nó trả lời một câu hỏi khác với câu được hỏi.

Cách sửa: chọn một cách sắp xếp mà S3 không tự trả lời được thì nạp nốt prefix vào
một bộ đệm riêng rồi mới tráo vào và sắp một lần. Trần 100 yêu cầu / 100.000 mục,
dừng được, và nếu bị cắt thì dòng trạng thái nói "sắp xếp trên phần đã tải".

### UI

1. ~~**Tab**~~ — xong (21/08/2026). Xem §9.1.
2. ~~**Ô đường dẫn `s3://bucket/prefix/`**~~ — xong (21/08/2026). Breadcrumb bấm
   vào là thành ô nhập, `⌘L` cũng vậy. Nhận cả dạng không có `s3://`, tự thêm dấu
   `/` cuối, và nếu đường dẫn ghi region khác profile thì nói ngay chứ không để
   request đi lạc endpoint.
3. ~~**Favorites và Recent**~~ — xong (21/08/2026). Lưu ở `places.json` cạnh
   profiles, gắn với profile id vì cùng một `bucket/prefix` dưới hai profile là
   hai nơi khác nhau. **Recent ra trang riêng** như Brows3 (21/08/2026): nằm
   trong sidebar thì trần thật sự là chiều cao sidebar, mà năm dòng thì không đủ
   để tìm ra cái gì còn dòng thứ sáu đã đẩy danh sách bucket rơi khỏi màn hình.
   Có trang riêng rồi thì trần thành 50, và mỗi nơi ghi luôn lần cuối ghé —
   `at` không nằm trong định danh của một nơi, nên ghé lại vẫn là *một* dòng
   chứ không phải dòng thứ hai. Nơi đã ghim vẫn ở sidebar: nó ít, cố định, và
   là thứ người ta cố ý để đó.
4. ~~**Nhãn loại tệp**~~ — xong (21/08/2026), nhưng ở **cột riêng** chứ không phải
   cạnh tên: một nhãn nằm sau tên thì mỗi dòng một vị trí, quét cả danh sách tìm
   "mấy cái CSV" là phải đọc từng dòng.
5. **Nút thao tác trên từng dòng** khi rê chuột, thay vì phải chọn rồi lên toolbar.
6. ~~**Chân danh sách nói rõ hơn**~~ — xong (21/08/2026). "8 mục · 5 thư mục,
   3 tệp" ngay dưới danh sách; thanh trạng thái nhường chỗ đó cho tên profile và
   region.
7. ~~**Màn hình Cài đặt**~~ — xong (21/08/2026). Chủ đề, giới hạn preview, băng
   thông, số luồng truyền, và đường tới thư mục cấu hình. Lưu ở `settings.json`.
8. **Bảng theo dõi API**: số request thành công/thất bại và log trực tiếp. Hợp với
   phần nhật ký lỗi ở §7.4.

### Tính năng

9. ~~**Sửa tệp tại chỗ**~~ — xong (21/08/2026). `Input` nhiều dòng trong modal
   xem trước, chỉ mở khi preview giữ *trọn* object, và giữ lại thẻ khi lưu.
10. ~~**Preview audio, video, PDF**~~ — quyết định không làm (21/08/2026).
    Xem trước chỉ dựng **ảnh và văn bản** (kể cả CSV/TSV thành bảng), có chủ ý:
    video cần bộ giải mã, đồng hồ và đường âm thanh dựng trên `gpui::surface`,
    PDF cần nhúng một thư viện dựng hình lớn — ký và đóng gói kèm — chỉ để phục
    vụ một khung. Mọi tệp đó đã có ứng dụng sẵn trên máy, nên thứ còn thiếu
    không phải bộ giải mã mà là **cánh cửa**: giờ mọi kiểu không dựng được đều
    ra chung một khung nói rõ đây là gì, vì sao không hiện, kèm "Mở bằng app" và
    "Tải xuống" — và không tải về một byte nào để nói câu đó.
11. ~~**Trần cho tìm kiếm sâu**~~ — xong (21/08/2026). 100 yêu cầu / 100.000 mục
    / 10.000 kết quả, và "chạm trần" là một trạng thái riêng chứ không lẫn vào
    "xong" hay "đã dừng".
12. ~~**Cache danh sách bucket**~~ — xong (21/08/2026). 30 phút, trong bộ nhớ,
    theo profile; sidebar nói rõ "từ bộ nhớ tạm" và có nút làm mới.
13. ~~**Chép đường dẫn `s3://`** và key~~ — xong (21/08/2026). Cả vùng chọn, mỗi
    key một dòng, thư mục cũng chép.
14. ~~**ACL đệ quy cho cả prefix**~~ — xong (21/08/2026). Đi hết prefix rồi chạy
    qua bộ chạy hàng loạt, có trần như tìm kiếm sâu. Chưa nhìn thấy chạy thật.
15. ~~**Xoá lùi về từng object**~~ — xong (21/08/2026). Chỉ lùi khi provider
    *không có* lệnh gộp, không lùi khi nó từ chối những key này.
16. ~~**Tự dò region**~~ — xong (21/08/2026). Từ `AWS_REGION` rồi `[default]`
    trong `~/.aws/config`, và lỗi sai region giờ nói luôn region đúng.

### Đề nghị không làm

- **Auto-update.** Vẫn chờ hạ tầng ký và chỗ host, đã ghi ở M10.
- **Nhúng một editor đầy đủ.** Monaco là một WebView; GPUI không có, và dựng lại
  một editor là một sản phẩm khác. Sửa text nhỏ thì được, còn "VS Code trong app"
  thì không.

### 9.1 Tab

Một tab là một vị trí đang duyệt, và giữ đủ thứ để quay lại thấy đúng chỗ đã rời:
bucket, prefix, danh sách đã tải, vùng chọn, con trỏ, bộ lọc, sắp xếp, vị trí
cuộn, và cả kết quả tìm kiếm nếu có.

**Chỉ tab đang mở giữ trạng thái sống.** Các tab khác giữ một bản chụp, đổi tab là
tráo bản chụp vào chỗ trạng thái sống. Cách này giữ nguyên khoảng 150 chỗ đang đọc
`self.entries`, `self.prefix`, `self.selection`… thay vì phải sửa hết thành
`self.tab().entries` — một lần refactor như thế trên 7500 dòng là mời lỗi vào nhà,
để đổi lấy đúng một thứ: các tab chạy nền cùng lúc, mà tải nền cho tab không nhìn
thấy thì cũng là tiền LIST tiêu cho cái không ai xem.

Đổi tab mà tab đó chưa có gì thì mới nạp; có rồi thì hiện lại bản chụp, không tốn
request nào.

Trùng chỗ thì nhảy tới tab đang mở chứ không mở thêm — Brows3 gọi đây là "smart
tab management" và nó đúng.

## 10. Kỹ thuật Brows3 có mà mình chưa có (21/08/2026)

§9 đối chiếu **tính năng**. Đây là đối chiếu **cách làm** — đọc `src-tauri/`,
`.github/workflows/release.yml`, `docs/RELEASE_SIGNING.md` và `update.json` của
họ. Xếp theo giá trị, không theo công sức.

### 10.1 Auto-update không cần chỗ host, cũng không cần tài khoản Apple

Đây là thứ đáng giá nhất, và nó **gỡ một cái chặn mình tự dựng lên**. M9 đang ghi
"auto-update cần nơi host và một cặp khoá ký"; Brows3 cho thấy cả hai đều không
phải rào:

- `update.json` **commit thẳng vào repo**, app đọc qua
  `raw.githubusercontent.com/<user>/<repo>/main/update.json`. Không có server nào
  cả.
- Bản dựng là **asset của GitHub Release**. Cũng không có server nào.
- Chữ ký cập nhật là một cặp khoá **minisign tự sinh tại máy**, khoá riêng nằm
  trong GitHub Secret, khoá công khai nằm trong file cấu hình đã commit. Nó
  **không liên quan gì tới Apple** — chỉ để app biết gói tải về đúng là của mình.
- macOS ký **ad-hoc** (`signingIdentity: "-"`), và có `docs/MACOS_TROUBLESHOOTING.md`
  hướng dẫn `xattr -rd com.apple.quarantine`. Ký Developer ID + notarize là
  **nhánh tuỳ chọn**, tự bật khi CI có đủ secret.

Nói cách khác: notarize chỉ cần cho việc *mở lần đầu êm ru*, không cần cho việc
*ship được bản cập nhật*. Mình đang chờ tài khoản Apple để làm cả hai, mà đúng ra
chỉ một cái cần chờ.

Phía mình không dùng Tauri nên không có `tauri-plugin-updater`; thứ tương đương là
`cargo-packager-updater` (đã nêu ở M9) hoặc Velopack. Hình dạng thì y hệt: một
manifest tĩnh + release asset + một cặp khoá tự sinh.

### 10.2 Không có CI nào cả

Repo mình chưa có `.github/`. Của họ: kiểm tra → tạo release → dựng ma trận 5 nền
(macOS arm64/x64, Linux x64/arm64, Windows) → ký → sinh `update.json` và cả
manifest winget → **kiểm lại asset** bằng script có unit test riêng
(`.github/scripts/*.test.js`). Kiểm asset sau khi dựng là chỗ đáng học nhất: một
release thiếu một nền là thứ chỉ người dùng nền đó phát hiện ra.

### 10.3 Nhớ region của từng bucket

`S3ClientManager` giữ `bucket_regions: HashMap<String, String>` và một hàm ghi
hàng loạt. Mình **dò** được region từ thông báo lỗi (§9.16) nhưng **không nhớ**:
lần sau vào lại bucket đó là lại một vòng lỗi-rồi-dò. Rẻ, và sửa đúng chỗ đang
tốn một request thừa mỗi lần.

### 10.4 Cache listing có trần thật

Họ cache kết quả một thư mục theo khoá `(profile, bucket, prefix, cột sắp, chiều
sắp)`, chặn **cả hai đầu**: tối đa 32 mục cache *và* tối đa 100.000 phần tử cộng
lại, đuổi cũ nhất trước, và thư mục nào một mình đã vượt trần thì không cache.
Mình mới cache danh sách bucket (TTL 30 phút). Đây cũng đúng chỗ §9 đang ghi là
"chưa đo": sắp xếp và lọc chạy trên toàn bộ `entries` trong RAM.

### 10.5 `[profile.release]` chưa đụng tới

Của họ: `panic = "abort"`, `lto = true`, `codegen-units = 1`, `opt-level = "s"`,
`strip = true`. `Cargo.toml` của mình chỉ chỉnh `[profile.dev]`. Lưu ý một chỗ
vướng: `panic = "abort"` không giết panic hook (`crash.rs` vẫn chạy) nhưng giết
`catch_unwind`, mà test của `crash.rs` đang dùng — test chạy ở profile test nên
không sao, chỉ là phải biết trước.

### 10.6 ~~Endpoint gõ thiếu `https://`~~ — xong (21/08/2026)

`normalize_endpoint_url` của họ thêm `https://` khi người dùng gõ trơ
`s3.example.com`. Mình không, và AWS SDK trả về "dispatch failure" — mà
`failure.rs` lại dịch thành "Không kết nối được tới endpoint" kèm nút **Thử lại**,
tức là mời người ta bấm mãi một nút không bao giờ chạy được, cho một lỗi gõ thiếu
tám ký tự.

Đã làm, và **khác họ một chỗ**: loopback thì thêm `http://`, còn lại `https://`.
Một object store ở `127.0.0.1:9000` mười lần thì chín là MinIO dev, mà MinIO dev
nói HTTP trần; đoán ngược lại chỉ là đổi một lỗi khó hiểu này lấy một lỗi khó
hiểu khác — bắt tay TLS với một server chưa từng có chứng chỉ. Cả dải 127/8,
`::1` trong ngoặc vuông, và `*.localhost` theo RFC 6761.

Chuẩn hoá ở **hai chỗ**: trong `split_endpoint` khi đọc form — thiếu scheme thì
không có gì để tách, nên `s3.example.com/mybucket` vừa dính bucket vào host vừa
tới SDK thiếu scheme, một lỗi gõ sinh hai lỗi — và trong `S3Client` khi dựng
client, cho `profiles.json` sửa tay hoặc do bản cũ ghi ra.

Địa chỉ LAN vẫn nhận `https` và vẫn hỏng nếu server đó là HTTP trần. Cố ý để vậy:
nhận diện dải riêng là 10/8, 172.16/12, 192.168/16 với cả `.local`, mà lỗi nó
tránh được ít ra cũng là một lỗi TLS **có tên**, không phải `dispatch failure`.

### 10.7 Đọc credential từ biến môi trường

Họ có `CredentialType::Environment` và `check_aws_environment` (đọc
`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_SESSION_TOKEN`/`AWS_REGION`).
Mình nhập được `~/.aws/*` nhưng bỏ qua biến môi trường — trong khi mọi công cụ AWS
khác đều đọc, và người dùng CLI mặc định là đã có sẵn ở đó.

### 10.8 Chỉ cho người dùng chỗ tìm log

Họ có lệnh `get_log_file_info` trả về đường dẫn log và `panic.log` để UI hiện lên.
Báo cáo crash của mình **tốt hơn** của họ (có backtrace, tách theo pid) nhưng
không có chỗ nào trong app nói nó nằm đâu — một file không ai tìm thấy thì cũng
như không viết.

### 10.9 Upload lỗi thì thử lại một lần ở region vừa dò

Họ: upload hỏng → dò region thật của bucket → nếu khác thì thử lại đúng một lần →
vẫn hỏng mới báo. Mình báo region đúng rồi để người dùng tự sửa. Hai cách đều
đúng; cách của họ đỡ một vòng thao tác cho trường hợp thường gặp nhất.

### 10.11 Sidebar: một hình dạng dòng duy nhất (21/08/2026)

Không phải học từ Brows3 mà là góp ý trực tiếp, nhưng cùng một chủ đề. Sidebar
đang có **ba** kiểu dòng và **hai** kiểu tiêu đề nhóm:

- Dòng điều hướng có icon, dòng bucket **không có** — nên chữ của bucket bắt đầu
  đúng chỗ *icon* của mọi dòng khác, để lại một mép trái lởm chởm chạy suốt cột.
- Danh sách đã ghim nhỏ hơn một bậc (`text_xs`, icon 11px), theo một lý thuyết
  về "danh sách con" mà nhìn vào chỉ thấy như một tai nạn.
- "ĐÃ GHIM" đệm `pt_2`, "BUCKETS" đệm `py_1` — lệch nhau vài pixel theo chiều
  dọc, loại khác biệt không ai gọi tên được mà ai cũng thấy.

Giờ tất cả đi qua một `sidebar_item`. Thêm icon `bucket` — bản đầu vẽ cái xô,
rồi đặt cạnh `trash.svg` mới thấy nắp cộng thân thuôn là đúng hình cái thùng
rác, nên vẽ lại thành hình trụ khoét vành trên.

### Không học

Monaco (là WebView, GPUI không có — xem "Đề nghị không làm" ở §9), MUI, Next.js.

### 10.10 Layout còn học được gì nữa

§9 lấy tab và modal xem trước. Đọc lại `components/layout/`, `navigation/PathBar`,
`app/page.tsx` và `app/uploads/page.tsx` thì còn:

**a. ~~Breadcrumb phải tự rút gọn~~** — xong (21/08/2026). Của họ `maxItems={5} itemsBeforeCollapse={2}`:
giữ đầu, `…`, giữ hai khúc cuối. Của mình vẽ **hết** mọi khúc trong một khung
`overflow_hidden`. Đã dựng thử `demo-bucket/du-an/khach-hang/2026/quy-3/thang-8/
hop-dong/ban-nhap/` — bảy cấp, không có gì lạ — và breadcrumb **ăn mất nút cuối
của toolbar**. Tệ hơn: cắt thì cắt phần **đuôi**, tức là đúng khúc nói mình đang
đứng đâu. `elide_middle` đã dùng cho nơi đã ghim ở sidebar rồi, chỉ là chưa dùng
ở đây.

**b. ~~Thanh đường dẫn gợi ý nơi vừa tới~~** — xong (21/08/2026). `⌘L` của họ là một autocomplete lấy
`recentPaths` làm options, mỗi dòng có icon đồng hồ, và cuối danh sách có link
"xoá lịch sử". Mình vừa dựng đúng cái kho dữ liệu đó (§9.3) mà thanh đường dẫn
vẫn gõ chay. Placeholder của họ còn dạy luôn cú pháp:
`s3://bucket@region/folder/`.

**c. ~~Danh sách bucket đáng có một *trang*~~** — xong (21/08/2026). Home của họ là
bảng: Bucket | Region | Created | Total Size. Mình làm **Tên | Ngày tạo**, và
thiếu hai cột kia là có lý do:

- **Total Size**: `total_size` phía Rust của họ **luôn là `None`** — một cột vĩnh
  viễn hiện `—`. Cột không bao giờ có dữ liệu là đồ đạc chết. Con số đó không về
  cùng `ListBuckets`, tính nó là đi hết mọi object trong mọi bucket.
- **Region**: SDK *có* trường `bucket_region`, nhưng tài liệu ghi rõ nó chỉ được
  điền "if the request contains at least one valid parameter" — `ListBuckets`
  trần thì `None`, và MinIO cũng không trả. Thành ra đúng cái bẫy vừa nói. Region
  theo từng bucket sẽ có nguồn thật khi làm §10.3, lúc đó thêm cột mới có nghĩa.

Ngày tạo thì `ListBuckets` trả sẵn và app đang vứt đi. Một cài đặt, hai hình
dạng (`list_buckets` gọi `list_buckets_detailed` rồi bỏ bớt) — hỏi hai lần cho
thứ một câu trả lời đã mang theo là loại lãng phí hiện lên hoá đơn.

**d. ~~Hàng đợi gom theo thư mục~~** — xong (21/08/2026). Gom theo
`(chiều, bucket, prefix cha)`, vì `Job` **không mang** thứ gì nói "hai tệp này
cùng một lần kéo thả". Nên hai tệp tải lên riêng lẻ vào cùng một thư mục cũng gom
chung — hướng sai an toàn, vì dòng mở ra là thấy đúng những tệp nào — và một thư
mục có thư mục con thì thành một dòng mỗi cấp.

**e. ~~Mục điều hướng mờ đi kèm tooltip~~** — xong (21/08/2026), nhưng **hẹp hơn
Brows3**. Trạng thái "chưa có profile nào" của họ ở app này không tới được
sidebar: chưa có profile thì cả sidebar không được vẽ, màn onboarding chiếm chỗ.
Trạng thái thật sự tới được là `client` rỗng — đọc keychain hỏng thì `connect`
thoát sớm trước khi đặt `active_profile`. Còn "khoá sai" thì `client` **vẫn có**
(dựng client không xác thực gì), và ở đó panel lỗi kèm nút "Sửa profile" nói được
nhiều hơn một tooltip. Nên cổng chặn khoá theo `client`, không theo
`active_profile` như bản agent làm — hai thứ đó lệch nhau đúng ở chỗ này. Chưa có profile thì
Home/Favorites/Recent/Downloads/Uploads vẫn nằm đó, xám, tooltip "Create a profile
first". Cách này dạy hình dáng ứng dụng trước khi dùng được nó.

**f. ~~Thanh trạng thái mang nhiều hơn~~** — xong một phần (21/08/2026): số phiên
bản ở thanh trạng thái, tuổi bộ nhớ tạm **ở sidebar cạnh danh sách bucket** chứ
không phải ở thanh trạng thái — `buckets_cached` đúng cho tới hết phiên, nên dưới
đó nó sống lâu hơn cái nó nói về, ngồi cạnh một danh sách object vừa lấy về xong
mà không nói gì về nó. Còn thiếu: đánh dấu region **tự dò** — phải chờ §10.3, vì
hiện app dò xong rồi quên, không có chỗ nào đọc lại được lúc vẽ.

Nguyên văn mục gốc: Trạng thái cache ("Cached 5m ago" /
"● Live"), một dòng nhắc tiền ("S3 API calls incur charges"), số phiên bản, và
region **hiện khác đi khi nó là do tự dò** chứ không phải do gõ vào. Mình có
profile · region · phím tắt.

**g. ~~Hộp thoại About~~** — xong (21/08/2026). Số phiên bản và **thư mục báo cáo
crash**, không phải link: link thì trình duyệt mở được từ đâu cũng được, còn một
tệp crash không ai tìm thấy là một tệp crash không ai gửi. Thư mục cấu hình không
lặp lại ở đây — nó đã có hàng riêng trong Cài đặt, ngay trên Giới thiệu ở chân
sidebar.

**h. ~~Ô lọc có nút `×` ngay trong ô~~** — xong (21/08/2026), và **tiền đề ban đầu
sai**. Lúc đối chiếu tôi kết luận là mình không có nút nào; thật ra cả hai ô lọc
đều đã gọi `.cleanable(true)` từ lâu, kèm nguyên một comment giải thích. Nhưng nó
**vẽ ra không có gì**: nút clear của gpui-component xin `IconName::CircleX`, mà
app này phục vụ bộ icon tự vẽ của riêng nó, không có tên đó. Nên suốt thời gian
qua có một control chiếm chỗ và vô hình — tệ hơn là không có, vì không ai đi tìm
thứ mình tưởng là không tồn tại.

Giờ tự dựng nút, đi qua `clear_filter` để focus quay về danh sách, và làm cho cả
ô lọc bucket ở sidebar. Nhân tiện gom ba bản sao của cùng một hình vuông-icon-nhỏ
(× trên tab, nút làm mới cạnh tiêu đề BUCKETS, và × mới) vào `small_icon_button`.

**i. ~~Gom theo trạng thái~~** — xong (21/08/2026): ĐANG CHẠY / XONG / HỎNG /
ĐÃ HUỶ, mục rỗng không vẽ gì. **Không** tách Uploads và Downloads thành hai trang
— một ngăn kéo có tiêu đề nhóm nói được cùng chừng ấy mà không bắt ai đổi trang
để biết một lần truyền đã xong chưa.

### 10.12 Cửa sổ hẹp và thanh trạng thái (21/08/2026)

Hai lỗi UI do người dùng chỉ ra, không phải từ đối chiếu Brows3.

**a. Inspector bị đẩy ra khỏi cửa sổ.** Khung object pane là `flex_1` nhưng
không có `min_w(0)` — mà sàn của một flex child là *nội dung* của nó, và nội dung
đây là một toolbar đầy nút cố định. Nên pane không chịu hẹp dưới khoảng 700px và
đẩy inspector ra khỏi mép phải: nhãn còn trên màn hình, **mọi giá trị biến mất**.
Thêm `min_w(0)` + `overflow_hidden` cho pane, `flex_shrink_0` cho inspector và
sidebar.

Sửa xong lộ ra lỗi thứ hai cùng họ: giờ pane hẹp được thì flexbox lấy chỗ từ
đúng ô **tên** — dòng đọc thành `6 ☐ 📄 BIN 2.9 MB`. Ô tên có `NAME_MIN_WIDTH`
làm sàn; cái gì phải mất ở bề rộng hẹp thì không phải là tên của thứ đang xem.

**b. Thanh trạng thái xếp lại.** Bốn gợi ý phím tắt (`⌘F ⌘N ⌘D ⌘J`) chiếm phần
lớn thanh, vừa cố định vừa **không đầy đủ**, và nhắc `⌘J` ngay cạnh chính cái nút
hàng đợi mà nó gọi tên. Palette đã in phím tắt của từng lệnh bên cạnh lệnh đó,
nên một đường vào là đủ: `⌘K lệnh`, bấm được.

Thêm vạch ngăn giữa các nhóm — trước đó profile, region, gợi ý, nút hàng đợi và
số phiên bản chạy liền thành một dải chữ mờ cùng cỡ. Tên profile đậm hơn region
một bậc: một cái là tài khoản nào, cái kia là chi tiết của nó. Và thứ duy nhất
được phép cắt ở giữa là **câu trạng thái** — nó là một câu, nửa câu vẫn nói được
điều gì đó; mọi thứ bên phải là control hoặc dữ kiện, nửa cái đó thì vô giá trị.

### 10.13 Cài đặt và hàng đợi (21/08/2026)

**Cài đặt chia nhóm.** Năm hàng phẳng — chủ đề, xem trước, băng thông, số luồng,
một đường dẫn — không có gì nói ba cái giữa là về việc chuyển byte còn cái cuối
là về máy này. Năm hàng là chỗ một danh sách phẳng thôi là danh sách và bắt đầu
là một đống. Giờ bốn nhóm: GIAO DIỆN / TRUYỀN TẢI / XEM TRƯỚC / DỮ LIỆU.

Cột nhãn rộng ra 210: ở 150 mọi ghi chú đều xuống hai ba dòng, mỗi hàng cao bằng
lời giải thích của nó, và sáu mục lấp đầy một nghìn pixel. Control canh trái
thành một cột chạy dọc được — như System Settings của macOS — chứ không kéo giãn
hết bề ngang, vì `flex_1` biến "Xoá và tải lại" thành thứ trông như ô nhập.

Thêm **Xoá bộ nhớ tạm** lấy từ Brows3: danh sách bucket nhớ 30 phút, nên một
bucket vừa tạo ở console không hiện ra cho tới khi hết hạn, mà đường duy nhất
vượt qua là nút làm mới cạnh tiêu đề sidebar — không phải chỗ ai đi tìm.

**Bỏ chip "Mã hoá" ở thanh hàng đợi.** Nó xoay vòng SSE cho mọi lần tải lên
(mặc định bucket → SSE-S3 → SSE-KMS). Bỏ theo yêu cầu; giờ tải lên dùng mã hoá
mặc định của bucket, tức là để **chính sách bucket quyết định** — vốn là chỗ nó
nên được quyết định. Mất khả năng chọn SSE-KMS với key riêng từ trong app. Vì đó
là đường vào duy nhất nên `FormKind::KmsKey`, `next_encryption` và
`encryption_label` cùng đi theo: code không đường nào tới còn tệ hơn code không
tồn tại. Phía `s3core` vẫn giữ `set_encryption` — nó là API của thư viện, có test
riêng, và không phải rác của app.

**Nút hàng đợi lên sidebar**, ngay trên Cài đặt. Trước nó là một pill trong thanh
trạng thái, kẹp giữa một dãy gợi ý phím tắt và số phiên bản — một control sống
trong dải duy nhất của cửa sổ vốn toàn chữ chỉ đọc. Nhãn kèm một con số khi có
việc đang chạy, hoặc "n lỗi" khi có việc hỏng; "xong" không phải một trạng thái
đáng đánh dấu, vì nó đúng mãi mãi. Tốc độ và thời gian còn lại vẫn ở thanh trạng
thái nhưng **chỉ khi đang chạy** — một hàng 214px không chứa nổi "2 đang chạy,
3 chờ, 5 MB/s, còn 2 phút".

### 10.14 Liquid glass (21/08/2026)

Yêu cầu: giao diện liquid glass, "bằng thuật toán". Nói thẳng giới hạn trước:
gpui 0.2.2 **không có backdrop-filter theo phần tử và không có shader hook**,
nên khúc xạ thật của Liquid Glass (Apple, macOS 26) là ngoài tầm — và test của
chính `theme.rs` cấm hộp thoại trong suốt, vì không có blur sau lưng thì nội
dung bên dưới xuyên thẳng qua chữ. Cái tính được là **ánh sáng**: một tấm kính
dưới nguồn sáng từ trên cao có vành sáng ở mép trên, một lớp sheen trượt xuống
mặt, độ dày đổ râm ở mép dưới, và ném một bóng sâu mềm. Mỗi thứ là một lớp rẻ.

Mô hình nằm ở `GlassSpec` trong `theme.rs` — mọi giá trị suy từ đúng một câu
hỏi: mode này thì nền và nguồn sáng nằm phía nào. Test ghim các phán quyết:
sheen ≤ 0.12 (quá nữa là sương mù đè nội dung chứ không phải ánh sáng trên
kính); keel luôn tối (keel sáng nghĩa là nguồn sáng từ dưới, mâu thuẫn mọi lớp
khác); vành ngược cực với nền (bắt sáng trên nền tối, cạnh cắt trên nền sáng);
vạch specular chỉ có ở dark — trắng-trên-trắng là vô hình, vẽ nó vẫn là đồ đạc;
bóng dark đậm hơn (panel gần màu nền), bóng light rộng và nhạt hơn.

Cọ vẽ là trait `LiquidGlass` trong `browser.rs`: một lời gọi thay bốn dòng style
ở 11 chỗ hộp thoại/popover; ba lớp sáng chèn làm con *đầu tiên* nên nội dung của
mọi caller vẽ đè lên trên. Bóng hai tầng: một tầng mềm sâu nói "đang nổi", một
tầng sát mép nói "có cạnh ở đây" — mỗi tầng đứng riêng đều đọc thành glow hoặc
sticker.

Tiện thể vá một bẫy lộ ra khi chụp màn hình: các hộp thoại panel (Cài đặt, Giới
thiệu, profile, nhật ký lỗi) không đóng bằng Escape — mở bằng bàn phím mà chỉ
chuột đóng được là cái bẫy hai cửa vào một cửa ra. Giờ Escape đóng, và ⌘K vẫn
xuyên qua được vì palette vẽ đè mọi hộp thoại theo thiết kế.

Một vòng review đối kháng tìm ra chỗ hay nhất của cả đợt: **sheen light mode bị
đảo dấu**. Shader gradient của gpui nội suy RGBA *thẳng* (không premultiply),
nên trắng phai về `transparent_black` đi **qua xám** giữa đường — dấu vết duy
nhất của "vệt sáng" trên hộp thoại trắng là một tấm màn *tối* phủ 38% trên.
Quy tắc rút ra, ghim vào cả comment lẫn test: **gradient phai về chính màu của
nó ở alpha 0**, không bao giờ về transparent đen; và sheen light về 0 hẳn theo
đúng luật đã áp cho vạch specular — trắng-trên-trắng là đồ đạc. Review cũng bắt
được: keel 10px tô lên hàng cuối của ba khung chạy sát đáy (popover gợi ý, thân
xem trước, danh sách palette) — thêm `liquid_glass_flush` không keel cho ba chỗ
đó; radius popover lệch giữa vỏ và lớp phủ — bỏ override `rounded_md`; và test
chỉ ghim trần chứ không ghim sàn, nên một spec toàn số 0 — không vẽ tí kính nào
— vẫn qua được bài test tự nhận là ghim mô hình ánh sáng. Đã thêm sàn alpha.

### 10.15 Liquid glass thật: fork gpui (21/08/2026)

§10.14 dừng ở "gpui không có backdrop-filter nên khúc xạ thật là ngoài tầm".
Câu hỏi tiếp theo là: kể cả sửa gpui? Trả lời: được, và đã làm. gpui giờ được
vendor tại `vendor/gpui` (8MB, Apache-2.0, patch qua `[patch.crates-io]`) với
đúng **một** năng lực mới: primitive `BackdropGlass` — đến lượt nó trong thứ tự
vẽ, renderer dừng lại, chụp khung hình đã vẽ ra texture, blur hai lượt gaussian
ở nửa độ phân giải, rồi vẽ tấm kính lấy mẫu texture đó: mặt nạ SDF bo góc,
**khúc xạ** đẩy toạ độ mẫu dọc gradient SDF trong dải một-blur-radius sát mép
(thấu kính thật gom cong ở vành, giữa tấm phẳng tuyệt đối), phủ tint. Đúng cơ
chế BackdropFilter của Flutter.

Vài chỗ đã phải trả giá để biết:

- Nhánh `Paths` của renderer **đã làm sẵn** động tác cắt pass giữa khung hình
  (end encoder → pass phụ → mở lại với `Load`) — không phải phát minh gì, chỉ
  nối thêm một bước blit + hai pass blur vào giữa.
- `CAMetalLayer.framebufferOnly` mặc định cấm đọc ngược drawable — tắt nó là
  giá vé vào cửa của mọi backdrop filter.
- cbindgen sinh `scene.h` từ struct Rust: `[f32; 2]` thành mảng C chứ không
  phải `float2`, shader phải tự ghép vector.
- NSGlassEffectView không phải đường tắt: cả UI nằm trong một CAMetalLayer,
  view kính native đặt lên trên sẽ đè lên *cả nội dung dialog*.

Phía app, `liquid_glass` bỏ nền đục: con đầu tiên giờ là một `canvas` gọi
`paint_backdrop_glass`, tint là màu modal ở alpha `frost` (dark 0.72, light
0.78 — light đậm sương hơn vì chữ tối cần nền vững hơn chữ sáng; test ghim sàn
0.6 và trần <1.0, vì frost 1.0 là cái panel đục trả tiền cho một cú chụp mà nó
che mất). Các lớp §10.14 (vành, sheen, keel, bóng) giữ nguyên bên trên — giờ
chúng là ánh sáng *trên* kính thật thay vì đứng thay kính.

Đã nhìn thật cả hai mode: hộp thoại Cài đặt và palette đều cho dòng danh sách
ghosting xuyên qua lớp sương; kính-chồng-kính (palette đè dialog) đúng thứ tự.
Chưa đo: chi phí GPU của capture+blur mỗi khung hình có kính (một blit + hai
pass nửa độ phân giải + N quad — dự là không đáng kể, nhưng là *dự*).

### 10.16 Từ sương đục thành kính (21/08/2026)

Bản đầu của §10.15 nhìn vẫn chưa "liquid": frost 0.72/0.78 trên nền blur là
**frosted plastic** — tấm nhựa mờ, không phải kính. Bốn thứ làm nên chất liệu
của Apple, giờ đều nằm trong fragment shader:

1. **Frost mỏng đi một nửa** (dark 0.45, light 0.52). Thứ gánh độ đọc của chữ
   ở frost thấp là lớp blur bên dưới, không phải lớp tint. Test đổi theo: sàn
   0.35, trần 0.6 — trần cũ 1.0 giờ chính là định nghĩa của cái sai cũ.
2. **Vibrancy**: đẩy bão hoà qua trung tính (×1.45) để màu phía sau bừng qua
   sương thay vì chết thành xám. Mọi material của Apple đều làm; blur không có
   nó là sương mù, không phải kính.
3. **Vành bẻ cong ảnh *sắc nét***: khúc xạ giờ lấy mẫu từ capture chưa blur
   (texture thứ hai đi kèm), ba tap lệch nhau một sợi tóc cho tán sắc — viền
   màu mờ ở đúng mép mà mắt không gọi tên được nhưng đọc ra "thấu kính".
   Khúc xạ trên ảnh đã blur chỉ ra nhão.
4. **Cả tấm là một thấu kính yếu**: backdrop phóng đại ~5% về tâm tấm. Nội
   dung *trượt khác tốc độ* dưới tấm so với cạnh tấm — cái này, hơn cả blur,
   là thứ đọc ra "một tấm vật liệu" thay vì một filter ảnh chụp.
   Kèm specular trong shader ở cạnh ngửa lên nguồn sáng (−normal.y).

Đã nhìn thật cả hai mode, kể cả kính-chồng-kính: palette nổi trên hộp Cài đặt,
đọc được chữ của dialog ghosting qua tấm, phóng nhẹ, mép mềm. Cũng dọn hai
warning `float_literal_f32_fallback` ở `taffy.rs` của upstream cho build sạch.

### 10.17 Tinh chỉnh kính + làm lại palette (21/08/2026)

Góp ý "chưa tinh tế": ba nguyên nhân nhìn ra được từ ảnh chụp. (1) Góc bo 8px —
Liquid Glass dùng góc lớn; lên 16px. (2) Hai hệ chiếu sáng cãi nhau: các dải
gradient *ngang* từ thời kính giả (sheen/edge/keel) vẫn đè lên ánh sáng *theo
SDF* của shader — dải thẳng trên vành cong là thứ đọc ra "mockup". Bỏ hẳn ba
lớp đó; chiếu sáng vành giờ sống trọn trong shader: sáng đúng chỗ cạnh ngửa
lên nguồn sáng (−normal.y), kể cả cung góc; một chút fresnel cho mọi cạnh; mặt
dưới trừ đi — độ dày đọc thành bóng râm. `GlassSpec` teo lại còn đúng thứ
shader không sở hữu được: vành hairline, bóng đổ, độ sương. (3) Vòng thấu kính
rộng bằng blur nên mép nhão: band co còn 0.6×, falloff bậc ba — vành lens gọn
ôm sát mép. Blur nâng 24→30.

Palette ⌘K làm lại theo góp ý "ui khá tệ" — chẩn đoán từ ảnh: header là chuỗi
trần dán sát mép trên (đọc như nhãn đi lạc, không phải chỗ gõ), dòng chọn tô
kín mép-tới-mép đè lên cung góc 16px (selection thò ra khỏi container của nó
đọc như lỗi render), shortcut là chữ mờ cạnh chữ mờ. Giờ: dòng tìm đúng dạng
dòng tìm (icon + chữ + caret đứng màu accent — palette thật sự nhận phím, mà
không có gì trên dòng đó nói thế), danh sách lùi lề `px_2` với dòng chọn bo
góc, shortcut thành keycap, footer dạy ba phím kèm đếm lệnh. Rộng 460→540 vì
nhãn dài nhất đụng keycap.

Đĩa lại xuống 751MB giữa chừng: `deps/` giữ rlib của mọi thế hệ build — ba bản
`libaws_sdk_s3` 174MB và 117 bản `libgpui`. Xoá hết trừ bản mới nhất, build
xác nhận vẫn link.

### 10.18 Kit control kính; frost mỏng nữa (21/08/2026)

Góp ý "trắng đục đục, chưa glass; chưa implement cho các nút". Hai việc:

**Frost 0.32/0.38** (từ 0.45/0.52). Liquid Glass thật tint chỉ quãng này; độ đọc
của chữ đến từ blur sâu (sigma 30→36) cộng vibrancy, không phải từ tấm màn
trắng. Trần trong test hạ theo về 0.45 — mỗi lần user chê đục là trần cũ thành
định nghĩa của cái sai cũ.

**Kit control chung** — mọi nút và chip cắt từ cùng một tấm kính: `control_base`
duy nhất (nang lồi: gradient dọc sáng trên tối dưới — độ cong chính là gradient
— vành hairline, hover là nắp bắt thêm sáng chứ không đổi màu), token
`control_top/bottom/border` trong Theme có test ghim chiều gradient ("gradient
lộn ngược đọc thành cái giếng, không phải cái nút"). `action_button`,
`action_button_dyn`, `setting_chip`, `choice_chip` (chip chọn: cùng nang, tô
màu selection + viền accent — selection là màu, không phải hình khối) và
`danger_button` (cắt từ kính đỏ, gradient quanh danger) đều đi qua kit. Nút
backdrop-glass thật cho từng control thì không làm: mỗi capture là một lần cắt
render pass, toolbar mười nút là mười lần blit+blur mỗi khung hình.

Trong lúc làm rơi `.child(label)` của choice_chip — cả hàng chip thành nang
rỗng, ảnh chụp bắt được ngay. Và ngừng đụng vào settings.json: file lật về
light lúc 23:10 khi app đang mở mà nguồn ghi duy nhất là click chuột vào chip —
tức là chính người dùng đang chọn theme trong lúc xem, còn tôi thì cứ "khôi
phục" đè lên lựa chọn đó.

### 10.19 Liquid Glass theo tài liệu, không theo phỏng đoán (22/08/2026)

Sau góp ý "tệ quá, research đi": ba agent research song song (HIG/WWDC25 của
Apple; các teardown có đo đạc — kube.io, ybouane, atlaspuplabs; các bản
implement có công thức — Kyant/AndroidLiquidGlass đối chiếu ảnh iOS 26 thật,
flutter liquid_glass_renderer, liquid-glass-studio) rồi một agent tổng hợp
thành spec số. Shader viết lại theo spec, và spec bắt đúng bốn chỗ bản cũ sai:

1. **Thấu kính toàn tấm 5% là "heat-shimmer fake".** Tiêu chí nghiệm thu số
   một của mọi teardown: đường thẳng sau tấm phải thẳng tuyệt đối trong ruột,
   chỉ được nén/bẻ trong vành ~24pt. Đã bỏ magnification; ruột phẳng quang học.
2. **Profile vành là cung tròn với đạo hàm 0 tại ranh trong**
   (`1 − √(1−x²)`), không phải luỹ thừa tuỳ tiện — đạo hàm 0 là thứ khiến chỗ
   vành gặp ruột không tự vẽ thành một vòng seam. Kéo mẫu VÀO TRONG (offset
   âm dọc gradient), không đẩy ra: vành thành bản nén của thứ ngay trong mép,
   đúng vành kính lúp. Không Snell/IOR — Kyant đối chiếu ảnh thật kết luận
   Apple dùng displacement thuần, các bản vật lý cho cùng đường cong với giá
   đắt hơn và thêm một tham số không ràng buộc.
3. **Highlight là hai thuỳ lệch 45°** (trên-trái sáng 1.0, dưới-phải 0.8, tối
   ở hai góc vuông góc, vệt ~0.5pt) — không phải vòng đều, không phải
   "−normal.y". Vòng đều là CSS border, không phải kính có đèn. Kèm inner
   shadow mờ phía quay lưng nguồn sáng.
4. **Tint dark ĐẬM hơn light** (0.40 so với 0.25) — ngược mọi phỏng đoán trước
   của phiên này: tấm tối không làm sáng backdrop lên được nên phải tint đi;
   test lật chiều với lý do ghi trong comment. Blur hạ về sigma 8pt (16 device
   px) — sương là voan, không phải tường; bão hoà đúng 1.5 trên luma Rec.709
   (Kyant lẫn Flutter cùng ship đúng 1.5). Tán sắc chỉ ở vành, ±6% trên chính
   offset nên tự về 0 ở ruột.

Nghiệm thu bằng chính ba bài test của spec, trên trang Gần đây (11 dòng chữ
dày làm nền, vì Docker liệt — mc treo — không dựng được listing MinIO):
đường dòng thẳng tuyệt đối dưới ruột tấm, chữ và icon màu ghosting qua sương,
nén nhẹ đúng ở vành, không viền cầu vồng, không răng cưa mép. Chưa soi được
dark mode trong phiên này (theme là lựa chọn đang mở của người dùng, không đè
nữa) — token dark đi cùng một shader, khác mỗi frost/tint đã ghim bằng test.

### 10.20 Kính thật xuống tận nút bấm (22/08/2026)

Phần còn thiếu của spec: control cũng là kính, không chỉ pane. Rào là chi phí —
mỗi batch kính là một lần cắt render pass + blit + hai pass blur, toolbar chục
nút là chục lần capture mỗi khung hình. Lối ra nằm ngay trong spec, là quy tắc
của chính Apple (WWDC25-323): **kính không bao giờ lấy mẫu kính khác** — nghĩa
là control chỉ cần những pixel *nằm dưới nó về mặt không gian*, mà thứ tự vẽ
của cây element đảm bảo chúng đã được vẽ trước. Vậy primitive thêm cờ `fresh`:
pane đòi capture tươi tại đúng lượt vẽ của nó (giữa hai pane là cả một màn nội
dung), control dùng chung capture sẵn có của khung hình — toolbar hai mươi nút
giá một lần capture, không phải hai mươi.

`control_base` giờ đặt một tấm kính shared dưới nắp: band 8px nên control gần
như toàn vành — đúng dáng viên thấu kính mà các catalog tham chiếu ship cho
nút. Frost control mỏng hơn pane (light 0.10, dark 0.20 theo bảng
reverse-engineer), có test ghim cả sàn lẫn quan hệ control < pane.

Bẫy thứ tự vẽ tốn một vòng: `.bg()` của element vẽ *trước* con, nên nắp
gradient và tint chọn của chip bị tấm kính (alpha 1 chỗ nó phủ) chôn mất — cả
hai phải thành lớp phủ vẽ *sau* kính, hover đi qua `group_hover` vì hover trên
div ngoài chỉ đổi cái nền đã bị chôn.

Nghiệm thu bằng mắt trên nền trang Gần đây: nhìn xuyên qua chip thấy chữ dòng
mờ phía sau; nắp, vành, ring accent của chip chọn còn nguyên; nhãn không bị
tint đè (lớp chọn chèn trước nhãn). Chưa đo được chi phí GPU — một capture
chung cho mọi control cộng một capture mỗi pane là thiết kế, nhưng con số là
điều còn nợ.

### 10.21 Revert liquid glass (22/08/2026)

Theo yêu cầu: trả giao diện về trước khi làm liquid glass, tính năng khác giữ
nguyên. Sau bốn vòng chỉnh (frost, kit, research, kính cho control), nhìn tổng
thể vẫn không đạt — vòng cuối control-kính-shared còn tự đục lỗ xuyên pane vì
đúng luật "kính không lấy mẫu kính". Cắt lỗ đúng chỗ:

**Bỏ:** `GlassSpec` + token control trong theme (checkout thẳng bản trước
kính); trait `LiquidGlass` — 11 dialog/popover về vỏ đục
`bg(modal) + border_strong` cũ; kit control — nút/chip về style phẳng cũ,
`BUTTON_HEIGHT` 22, `FIELD_HEIGHT` 26; **fork gpui** — gỡ `vendor/gpui` và
`[patch.crates-io]`, build lại trên gpui crates.io, vì không còn ai gọi
`paint_backdrop_glass` thì fork 8MB là gánh nặng chết. Toàn bộ nằm trong lịch
sử git (3e48c59…f791ccd) nếu có ngày quay lại — cùng spec research §10.19 vẫn
nguyên giá trị.

**Giữ:** palette ⌘K bản làm lại (dòng tìm + keycap + footer); Escape đóng các
dialog panel và ⌘K xuyên qua; mọi thứ trước mốc kính (sửa cửa sổ hẹp, thanh
trạng thái, trang Gần đây / Tất cả bucket, hàng đợi gom nhóm…).

Đã soi lại: hộp Cài đặt nền đục trắng, chip phẳng như cũ; palette giữ layout
mới trên nền đục. 174 test pass trên gpui nguyên bản.

### 10.22 Bo góc modal, và một lỗi tự gây ở §10.21 (22/08/2026)

Góp ý "modal hơi vuông" hoá ra bắt đúng một lỗi của chính đợt revert. Script
revert phân biệt vỏ dialog với vỏ popover bằng mức thụt lề, và **điều kiện bị
ngược**: mười hộp thoại nhận vỏ popover (`rounded_md`, 6px) còn cái popover duy
nhất nhận vỏ dialog. Bản trước kính có đúng mười `rounded_lg` + một
`rounded_md`; đối chiếu với `git show 9fd6290` là thấy ngay.

Sửa lại đúng chiều, và nhân đó bo tròn hơn theo ý: hộp thoại **12px**
(`rounded_xl`, từ 8px), popover **8px** (`rounded_lg`, từ 6px). 12px là quãng
sheet của macOS hiện nay, và ở kích thước hộp thoại thì 8px vẫn đọc ra hơi
vuông.

Bài học ghi lại: khi revert bằng script, thứ phân biệt hai khối phải là *nội
dung* của chúng, không phải khoảng trắng — thụt lề là thứ dễ đảo nhất và im
lặng nhất khi đảo.

### 10.23 Tải để "Mở bằng app" có mặt trên màn hình (23/08/2026)

"Mở bằng app" tải cả tệp về rồi mới giao cho ứng dụng khác, và trong lúc đó
phản hồi duy nhất là một dòng ở thanh trạng thái. Với tệp 200 MB nghĩa là hàng
chục giây không có gì chuyển động, không biết còn bao lâu, không có đường lùi.

**Chip nổi thay vì dòng trạng thái.** "Mở bằng app" được gọi từ overlay xem
trước cũng nhiều như từ danh sách, mà scrim của overlay đó phủ lên thanh trạng
thái — một dòng không ai nhìn thấy thì không phải là phản hồi. Chip nằm góc
dưới phải, trên preview và các hộp thoại, dưới palette: tên tệp, thanh 4px,
`{đã tải} / {tổng}` và phần trăm, cộng một dấu × để huỷ.

**Tải theo khối 4 MB.** Một GET cả object chỉ có đúng hai trạng thái quan sát
được, và không cái nào là "sắp xong". Tiến độ đi qua hai `AtomicU64` chia sẻ với
task; vòng repaint 125ms của hàng đợi được nới điều kiện để chạy tiếp khi đang
có chip, nên không cần cơ chế vẽ lại thứ hai.

**Ổ cắm task riêng.** Trước đây dùng chung `op_task`, nghĩa là bất kỳ thao tác
nào khác cũng huỷ mất lượt tải — và sẽ để lại cái chip đứng im với không có gì
phía sau. Cùng lý do `caps_task` và `thumb_task` từng được tách ra.

**Một lỗi im lặng sửa luôn.** Kích thước lấy từ listing, `unwrap_or(0)`; khi
object được mở từ chỗ không có listing đứng sau thì nó rơi xuống `0..0` và ghi
ra **một tệp rỗng**, rồi giao cho ứng dụng khác — hiện ra như tài liệu hỏng chứ
không phải như một lỗi. Giờ size bằng 0 nghĩa là "chưa biết", và task hỏi
`HeadObject` trước.

Huỷ thì xoá luôn tệp dở. Lần mở sau vẫn ghi đè, nhưng một tệp dở mang đúng tên
một object thật là thứ về sau có người mở tay ra và tin.

**Chưa biên dịch** (đang giữ đĩa trống). Mới chỉ chắc chắn cú pháp hợp lệ:
`rustfmt` phân tích được toàn bộ file.

### 10.24 Ô kiểu tệp ở panel chi tiết (23/08/2026)

Góp ý "icon file type nên chỉn chu hơn" bắt đúng bốn thứ cùng lúc, và chỉ một
trong số đó là chuyện thẩm mỹ.

**Hai bản sao đã lệch nhau.** Ô ở panel chi tiết và ô ở overlay xem trước là hai
khối giống hệt nhau chép ra — trừ việc một cái có `text_xs`, cái kia không. Cùng
một huy hiệu, hai cỡ chữ, hai chỗ. Giờ là một `kind_tile` dùng chung.

**Chữ không vừa ô.** `PNG` ở 12px trong ô 30px chạm cả hai mép; `WEBP` hay
`SQLITE` thì tràn. Cỡ chữ giờ bậc theo độ dài (11 / 10 / 8,5 / 7,5 / 7px) —
*bậc chứ không cắt*: `SQLITE` rút thành `SQLI` trông như lỗi vẽ, và nó đặt cho
tệp một cái kiểu mà tệp không có, trong chính cái ô sinh ra để gọi tên kiểu.

**Ô 30px cạnh một dòng chữ 12px.** Ở panel chi tiết, tệp đơn chỉ có một dòng
tên, nên ô là thứ cao nhất hàng, cao hơn nút đóng 26px. Xuống 26px, và thêm
một đường viền tóc: `hover` là lớp phủ 6%, ở theme sáng nó **không có cạnh
riêng** — đó là lý do chính khiến nó đọc ra như một lỗ khoét chứ không phải một
vật đặt trên panel.

**Hai nghĩa, hai hình.** Khi panel mô tả một tập chọn thì ô đó chứa con số. Một
con số nằm trong ô kiểu-tệp đọc ra là "kiểu tệp tên là 3". Số giờ nằm trong hình
tròn, chữ nằm trong hình vuông.

Bỏ luôn chuỗi dự phòng `TỆP`: tệp không có phần mở rộng thì không phải là tra
cứu thất bại, nên nó không được gán chữ — nó nhận glyph tờ giấy. Dấu chồng của
`Ệ` vốn là thứ cao nhất trong ô.

Không thêm màu theo họ tệp. Cả app đang là trung tính + một màu nhấn, và bảy sắc
độ cho bảy họ tệp là quyết định thị giác cần nhìn tận mắt trước khi chốt, không
phải thứ đoán mò khi chưa build được.

**Chưa biên dịch**, mới chắc cú pháp (`rustfmt` phân tích trọn file).

### 10.25 Title bar tự vẽ, không dựa vào native (23/08/2026)

Windows và Linux không có nút đóng/thu nhỏ/phóng to. Nguyên nhân **không phải**
là thiếu title bar — mà là đúng cái bẫy file này đã dính hai lần trước.

`TitleBar` của gpui-component *có* vẽ ba nút, bằng `IconName::WindowClose`, tức
`icons/window-close.svg`. App này phục vụ bộ icon tự vẽ của chính nó, không có
tên đó, nên `Assets::load` trả `None`. **Ba cái nút vẫn ở đó**: vẫn chiếm 34px
mỗi cái, vẫn đăng ký `WindowControlArea` nên Windows vẫn bấm được — và vô hình.
Không lỗi, không log. Giống hệt `cleanable(true)` ở §10.10h và giống hệt lý do
`menu_icon` phải tồn tại.

Sửa bằng cách **bỏ hẳn `TitleBar`** và tự dựng, theo đúng yêu cầu: icon của
mình, theme của mình, kích thước của mình. Ba icon mới (`window-minimize`,
`window-maximize`, `window-restore`); nút đóng dùng lại `close` sẵn có, vì một
chữ X thứ hai vẽ hơi khác chữ X ở mọi chỗ khác là thứ không ai muốn.

**Một phát hiện định hình cả layout.** Windows trả lời `WM_NCHITTEST` bằng vùng
control **đầu tiên** chứa con trỏ, mà các vùng được đăng ký theo thứ tự vẽ — cha
trước con. Một vùng kéo phủ cả hàng vì thế **nuốt mọi cú bấm** dành cho ô path
và chip profile nằm trong nó: hitbox của hàng được tìm thấy trước, OS được bảo
là con trỏ đang ở trên caption. Nên vùng kéo là một dải riêng giữa ô path và
chip, và ô path bị chặn ở 560px để dải đó luôn có chỗ tồn tại.

Chia việc theo nền tảng, mỗi chỗ một lý do:

| | kéo cửa sổ | bấm nút |
|---|---|---|
| Windows | `WindowControlArea::Drag`, OS tự kéo | OS tự xử lý, click không tới app |
| Linux | `start_window_move()` khi *di chuyển* chứ không phải khi nhấn — nhấn là nuốt mất cú double-click | `on_click` của mình |
| macOS | traffic light gốc, không vẽ gì | — |

Chuột phải trên dải kéo mở `show_window_menu` (Linux). Viền kéo giãn thì `Root`
của gpui-component đã bọc `window_border()` sẵn, không thiếu.

Thêm `S3BROWSER_WINDOW_CONTROLS=0|1` cùng tinh thần với `S3BROWSER_GLASS`: đó là
cách soi ba cái nút trên máy Mac mà không phải cross-compile sang nền tảng thật
sự cần chúng.

Và một test chặn đúng loại lỗi này tái phát: duyệt mọi `WindowButton` rồi khẳng
định `Assets::load` trả về `Some` cho icon của nó. Nếu ai đó đổi tên icon, test
đỏ — thay vì cửa sổ lặng lẽ mất nút.

**Hai lỗi tiếp theo, và cách tìm ra (23/08/2026).** Bản đầu vẫn không có icon.

Nguyên nhân là tôi tái tạo đúng cái bug đang sửa, thấp hơn một tầng. Tôi bỏ
`text_color` trên svg để hover đổi màu được — nhưng `vendor/gpui/src/elements/svg.rs`
làm thế này:

```rust
if let Some((path, color)) = self.path.as_ref().zip(style.text.color) {
```

Thiếu màu thì nó **không vẽ gì cả**, và `compute_style` dựng style từ
`Style::default()` rồi refine bằng `base_style` của chính phần tử — màu chữ của
cha *không* cascade xuống. Ba cái nút lại vô hình, lần này do tôi. Sửa: svg có
màu nền của riêng nó, còn sắc hover đặt bằng `group_hover` khoá theo group của
nút — vì con trỏ nằm trên nút 40px chứ không nhất thiết trên 14px glyph.

Lỗi thứ hai: `window-restore` tôi vẽ tay ra một khối đặc chứ không phải nét
viền. Tìm ra bằng cách **rasterise rồi nhìn**: `qlmanage -t -s 256 -o <dir>
<file>.svg` sinh PNG, và PNG thì đọc được. Vẽ lại theo lối hai ô vuông kinh
điển rồi render lại để xác nhận. Kỹ thuật này đáng nhớ — từ nay icon tự vẽ
không phải đoán nữa.

Và khi ép `S3BROWSER_WINDOW_CONTROLS=1` trên macOS thì có **hai** bộ nút cửa
sổ: traffic light của hệ thống bên trái, ba nút của app bên phải. Hai bộ trên
một thanh không phải bản xem thử của nền tảng chỉ có một bộ, nên khi bật cờ,
traffic light bị đẩy ra ngoài cửa sổ và `toolbar_leading_inset` tụt từ 88 xuống
12 — chỗ trống 88px vốn để chừa cho những cái nút giờ không còn ở đó.
