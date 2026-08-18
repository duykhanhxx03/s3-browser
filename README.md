# s3browser

Desktop S3 client viết bằng Rust + [GPUI](https://gpui.rs), UI glass kiểu macOS, macOS-first.
Kế hoạch đầy đủ: [PLAN.md](PLAN.md).

**Trạng thái: M0 (spike khả thi) đã xong — 4/4 gate pass.** Xem [Kết quả M0](#kết-quả-m0).

## Chạy thử

```bash
scripts/minio-dev.sh start   # MinIO local + dữ liệu mẫu (cần Docker)
cargo run -p s3browser
```

Cờ hữu ích:

| Cờ | Tác dụng |
|---|---|
| `--stress` | Mở list 100.000 dòng giả để đo ảo hóa |
| `--verify-glass` | Hỏi AppKit xem hiệu ứng glass có thật sự được gắn, in báo cáo rồi thoát (exit ≠ 0 nếu fail) |

```bash
cargo test                                    # unit + UI + integration (test MinIO tự bỏ qua nếu chưa chạy Docker)
cargo run -p s3browser -- --verify-glass      # kiểm tra glass
```

## Yêu cầu môi trường

- Rust stable (đã thử trên 1.97.1)
- Xcode + **Metal Toolchain**: GPUI compile shader Metal lúc build. Xcode 26 tách component này ra, thiếu nó `cargo build` sẽ báo `cannot execute tool 'metal'`. Cài một lần:
  ```bash
  xcodebuild -downloadComponent MetalToolchain
  ```
- Docker (chỉ để chạy MinIO khi dev/test)

## Cấu trúc

```
crates/
├── app/        # GPUI: cửa sổ, view, glass self-check
├── s3core/     # Bọc aws-sdk-s3, không phụ thuộc UI → test được không cần cửa sổ
└── gpui_tokio/ # Vendor từ Zed: cầu nối Tokio ↔ executor của GPUI
```

`s3core` cố tình không biết gì về GPUI: toàn bộ logic S3 test được bằng `cargo test` thường,
và sau này dùng lại cho CLI companion.

## Kết quả M0

Bốn điều kiện chốt stack trong plan, cùng bằng chứng đo được:

| Gate | Kết quả | Bằng chứng |
|---|---|---|
| Glass UI | ✅ | `--verify-glass` hỏi thẳng AppKit: `isOpaque=false`, `backgroundColor.alpha=0.0001`, `FullSizeContentView`, `titlebarAppearsTransparent=true`, và **`BlurredView` (subclass `NSVisualEffectView`) có thật trong view hierarchy** |
| List ảo hóa 100k | ✅ | `--stress`: `built rows 0..22 of 100000` — chỉ 22 phần tử được dựng |
| Drop file từ Finder | ✅ | Test `accepts_a_drop_from_finder` mô phỏng `FileDropEvent` thật, listener chạy và đổi state |
| AWS SDK trên Tokio | ✅ | Trong tiến trình GUI: `connected via gpui_tokio, 2 buckets` → `listed 5 entries (3 folders)`; thêm 3 integration test đối chiếu MinIO |

### Ba điều phát hiện khi làm, khác với nghiên cứu ban đầu

1. **`gpui_tokio` upstream không build được với gpui 0.2.2.** Nó nhắm bản git của Zed: dùng
   `App::background_spawn` (chưa có ở 0.2.2) và generic `AppContext` (ở 0.2.2 `Result` là
   associated type nên không unify được). Bản vendor đã sửa: nhận `&App` (`&mut Context<T>` tự
   deref), spawn qua `background_executor()`. Ghi chú ngay trong file — đây đúng là kiểu churn
   pre-1.0 mà plan đã lường trước.

2. **`ExternalPaths` không dựng được từ crate khác** ở 0.2.2 (field `pub(crate)`, không có
   `From`). Test drop chỉ gửi được payload rỗng, đủ chứng minh routing + listener, còn phần
   platform điền đường dẫn thì do gpui tự test. Nếu sau này cần test payload thật, phải dùng
   bản git hoặc test thủ công.

3. **Metal Toolchain là dependency ẩn** (688 MB) — không có trong tài liệu GPUI, phải thêm vào
   hướng dẫn onboarding, xem mục [Yêu cầu môi trường](#yêu-cầu-môi-trường).

### Chưa làm trong M0 (đúng phạm vi spike)

Phân trang khi cuộn, profile manager + Keychain, upload thật khi thả file (mới chỉ nhận đường
dẫn), theme sáng, và mọi thứ từ M1 trở đi trong [PLAN.md](PLAN.md).
