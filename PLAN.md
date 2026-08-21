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
