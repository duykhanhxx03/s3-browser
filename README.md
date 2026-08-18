# s3browser

Desktop S3 client viết bằng Rust + [GPUI](https://gpui.rs), UI glass, chạy trên macOS / Windows / Linux.
Kế hoạch đầy đủ: [PLAN.md](PLAN.md).

**Trạng thái: M1 (Browse) đã xong.** Xem [Kết quả M1](#kết-quả-m1) · [Kết quả M0](#kết-quả-m0).

## Chạy thử

```bash
scripts/minio-dev.sh start --large    # MinIO local + dữ liệu mẫu (cần Docker)
cargo run -p vault --example dev_profile   # tạo sẵn profile "MinIO local"
cargo run -p s3browser
```

Lần đầu chạy mà chưa có profile nào, sidebar sẽ hiện hai nút: **Nhập từ ~/.aws** và **Thêm MinIO local**.

### Phím tắt

`⌘`/`Ctrl` tuỳ theo hệ điều hành.

| Phím | Việc |
|---|---|
| `⌘F` | Lọc theo tên (gõ để lọc dần, Esc để xoá) |
| `⌘N` / `⌘⇧N` | Thư mục mới / bucket mới |
| `⌘R` | Tải lại prefix hiện tại |
| `⌘⌫` | Xoá các mục đang chọn |
| `⌘↑` | Lên một cấp |
| Nhấp đúp | Vào thư mục |
| `⌘`+nhấp | Chọn thêm |

### Cờ dòng lệnh

| Cờ | Tác dụng |
|---|---|
| `--open bucket/prefix/` | Mở thẳng một vị trí khi khởi động |
| `--verify-glass` | Hỏi AppKit xem hiệu ứng glass có thật sự được gắn, in báo cáo rồi thoát (macOS) |
| `S3BROWSER_DEBUG=1` | Bật log chẩn đoán (kết nối, số mục, phân trang) |
| `S3BROWSER_GLASS=0/1` | Ép chế độ solid / glass, ghi đè mặc định theo platform |

```bash
cargo test    # 28 test; test MinIO tự bỏ qua nếu chưa chạy Docker
```

## Yêu cầu môi trường

Chung: Rust stable (đã thử trên 1.97.1). Docker chỉ cần khi dev/test với MinIO.

| Nền tảng | Thêm |
|---|---|
| macOS | Xcode + **Metal Toolchain**. GPUI compile shader Metal lúc build; Xcode 26 tách component này ra, thiếu nó `cargo build` báo `cannot execute tool 'metal'`. Cài một lần: `xcodebuild -downloadComponent MetalToolchain` (688 MB) |
| Windows | Bộ build MSVC (Visual Studio Build Tools). GPUI dùng Direct3D + DirectWrite |
| Linux | Thư viện phát triển Wayland/X11 + Vulkan loader (GPUI đã chuyển renderer sang wgpu) |

## Cross-platform

Mọi khác biệt nền tảng gom trong [crates/app/src/platform.rs](crates/app/src/platform.rs), không rải rác trong code UI:

| Điểm khác | macOS | Windows | Linux |
|---|---|---|---|
| Nền cửa sổ | Blur thật (NSVisualEffectView) | Blur = Acrylic | **Solid** — blur chỉ có trên KDE, nên mặc định đục cho dễ đọc; bật bằng `S3BROWSER_GLASS=1` |
| Traffic lights | Dời vào trong, toolbar chừa 88px | Nút hệ thống, chừa 12px | Nút hệ thống, chừa 12px |
| Font | SF Pro Text | Segoe UI Variable | Inter / Cantarell / Ubuntu / DejaVu |
| Phím chính | `⌘` (`modifiers.platform`) | `Ctrl` | `Ctrl` |
| Credential store | Keychain | Credential Manager | Secret Service |
| Thư mục config | `~/Library/Application Support/s3browser` | `%APPDATA%\s3browser` | `~/.config/s3browser` |

Theme không có bảng màu riêng cho từng chế độ: chỉ **nền cửa sổ** đổi giữa trong suốt (glass) và đục (solid), mọi panel phía trên đều là lớp alpha nên hiển thị đúng ở cả hai. Sáng/tối bám theo hệ thống ngay khi người dùng đổi, qua `observe_window_appearance`.

## Cấu trúc

```
crates/
├── app/        # GPUI: cửa sổ, view, theme, platform, glass self-check
├── s3core/     # Bọc aws-sdk-s3 — không phụ thuộc UI, test được không cần cửa sổ
├── vault/      # Profile (JSON) + secret (credential store OS) + import ~/.aws
└── gpui_tokio/ # Vendor từ Zed: cầu nối Tokio ↔ executor của GPUI
```

`s3core` và `vault` cố tình không biết gì về GPUI, nên logic S3 và quản lý profile test được bằng
`cargo test` thường và sau này dùng lại cho CLI companion.

## Kết quả M1

Đã có: quản lý nhiều profile với secret trong credential store của OS; import `~/.aws/credentials`
+ `config` (ghép `[profile x]`, bỏ qua profile SSO, tự đặt quirk theo endpoint); sidebar profile +
bucket; breadcrumb bấm được từng cấp; sắp xếp theo tên/kích thước/ngày (thư mục luôn đứng trước);
lọc theo tên; **phân trang tự động khi cuộn** (continuation token); tạo thư mục và bucket; xoá
nhiều mục (mở rộng thư mục đệ quy, batch 1000 key); sáng/tối theo hệ thống.

Điểm cần biết về cách làm:

- **Chống race khi điều hướng**: mỗi lần chuyển prefix tăng một `generation`; phản hồi về trễ của
  prefix cũ bị bỏ qua thay vì ghi đè danh sách đang xem.
- **Lọc không làm mất dữ liệu**: `visible` chỉ là danh sách chỉ số vào `entries`, nên xoá bộ lọc là
  khôi phục ngay, và không phải tải lại.
- **Nhập liệu**: gpui 0.2.2 không có ô nhập sẵn, nên bộ lọc và hai ô đặt tên dùng chung một cơ chế
  bắt phím (`key_char` để đúng với bàn phím không phải US). Sẽ thay bằng `Input` của gpui-component.
- **Nhấp đúp**: gpui 0.2.2 không có `on_double_click`, nhưng `ClickEvent` mang `click_count`.

## Kết quả M0

Bốn điều kiện chốt stack trong plan, cùng bằng chứng đo được:

| Gate | Kết quả | Bằng chứng |
|---|---|---|
| Glass UI | ✅ | `--verify-glass` hỏi thẳng AppKit: `isOpaque=false`, `backgroundColor.alpha=0.0001`, `FullSizeContentView`, `titlebarAppearsTransparent=true`, và **`BlurredView` (subclass `NSVisualEffectView`) có thật trong view hierarchy** |
| List ảo hóa 100k | ✅ | Chỉ 22 phần tử được dựng trên 100.000 dòng |
| Drop file từ Finder | ✅ | Test mô phỏng `FileDropEvent` thật, listener chạy và đổi state |
| AWS SDK trên Tokio | ✅ | Trong tiến trình GUI: `connected: 2 buckets` → `listed 1000 entries, more=true` |

### Những thứ khác với nghiên cứu ban đầu

1. **`gpui_tokio` upstream không build được với gpui 0.2.2** — nó nhắm bản git của Zed: dùng
   `App::background_spawn` (chưa có ở 0.2.2) và generic `AppContext` (ở 0.2.2 `Result` là associated
   type nên không unify được). Bản vendor đã sửa: nhận `&App`, spawn qua `background_executor()`.
2. **`ExternalPaths` không dựng được từ crate khác** ở 0.2.2 (field `pub(crate)`, không có `From`).
   Test drop chỉ gửi được payload rỗng, đủ chứng minh routing + listener.
3. **Metal Toolchain là dependency ẩn** (688 MB), không có trong tài liệu GPUI.

## Chưa làm

Tải lên thật khi thả file (hiện mới nhận đường dẫn), tải xuống, hàng đợi truyền tải với
pause/resume — đó là M2. Đổi tên/di chuyển, presigned URL, versioning, ACL: M3–M4. Xem
[PLAN.md](PLAN.md).
