# Đóng gói và phân phối

Phân phối trực tiếp, không qua App Store. Lý do đã ghi ở PLAN.md §5: App Sandbox
siết drag-drop và bookmark, còn GPUI chưa có accessibility nên rủi ro bị từ chối
khi review.

## Trạng thái

| Bước | Trạng thái |
|---|---|
| Cấu hình `cargo-packager` | Xong, ở `crates/app/Cargo.toml` |
| Dựng `.app` và `.dmg` chưa ký | Xong, đã chạy thật — `.dmg` 12 MB, `LSMinimumSystemVersion` vào đúng plist |
| Ký Developer ID | **Chưa làm được** — cần chứng chỉ mà máy này không có |
| Notarize | **Chưa làm được** — phụ thuộc bước trên |
| Auto-update | **Chưa làm** — cần nơi host và khoá ký |

Ba dòng cuối không phải chuyện code. Chúng cần một tài khoản Apple Developer trả
phí (99 USD/năm) và một chỗ host, tức là quyết định của chủ dự án chứ không phải
thứ viết thêm được vào repo.

## Dựng bản chưa ký

`cargo packager` **không tự build** — nó chỉ đóng gói thứ đã có. Chạy thẳng sẽ
báo "No such file or directory" trỏ vào file binary chưa tồn tại:

```bash
cargo build --release -p s3browser
cargo packager --release -p s3browser
```

Kết quả ở `target/release`: `s3browser.app` và `s3browser_<version>_aarch64.dmg`
(khoảng 12 MB).

### Phải ký lại ngay sau khi đóng gói, kể cả để chạy thử

Bundle vừa dựng xong **không hợp lệ** về mặt chữ ký. Kiểm tra sẽ ra:

```
code has no resources but signature indicates they must be present
```

Nguyên nhân: linker đã ký sẵn file binary (ad-hoc, "linker-signed"), nhưng chữ
ký đó ký cho một file đơn lẻ chứ không cho cấu trúc bundle bọc quanh nó sau đó.
Ký ad-hoc lại là xong:

```bash
codesign --force --deep --sign - target/release/s3browser.app
```

Sau bước này `codesign --verify --deep --strict` báo "valid on disk" và
"satisfies its Designated Requirement".

### Nhưng vẫn chưa phân phối được

Ký ad-hoc xong, Gatekeeper vẫn từ chối:

```
$ spctl --assess --type execute --verbose=4 target/release/s3browser.app
target/release/s3browser.app: rejected
```

Đây chính là bằng chứng cho phần dưới: chữ ký hợp lệ về cấu trúc **không đồng
nghĩa** với được phép phân phối. Chỉ Developer ID mới qua được `spctl`.

Máy khác tải bản này về sẽ thấy thông báo đại ý "không mở được vì không rõ nhà
phát triển". Người dùng có thể chuột phải → Mở để vượt qua, nhưng đừng coi đó là
cách phân phối: phần lớn sẽ dừng ở thông báo đầu tiên và nghĩ app bị hỏng.

## Chứng chỉ: hai loại khác nhau, dễ nhầm

Máy này đang có:

```
Apple Development: <email> (<team>)
```

**Đây không phải chứng chỉ để phân phối.** Apple Development chỉ dùng để chạy thử
trên máy của chính mình. Thứ cần cho phân phối ngoài App Store là **Developer ID
Application**, và nó chỉ cấp cho tài khoản thuộc Apple Developer Program trả phí.

Kiểm tra máy đang có gì:

```bash
security find-identity -v -p codesigning
```

Nếu kết quả không có dòng nào bắt đầu bằng `Developer ID Application:` thì chưa
ký phân phối được, dù có bao nhiêu chứng chỉ khác đi nữa.

## Khi đã có Developer ID

Ký, kèm hardened runtime — notarize bắt buộc phải có cờ này:

```bash
codesign --force --deep --options runtime --timestamp --sign "Developer ID Application: TÊN (TEAMID)" target/release/s3browser.app
```

Kiểm lại trước khi gửi đi, vì notarize thất bại vì lý do này thì thông báo lỗi
rất khó đoán:

```bash
codesign --verify --deep --strict --verbose=2 target/release/s3browser.app
```

## Notarize

Cần một App Store Connect API key (Issuer ID, Key ID, file `.p8`). Dùng khoá API
thay vì mật khẩu app-specific vì khoá không gắn với mật khẩu Apple ID của một
người, nên không chết khi người đó đổi mật khẩu hoặc rời dự án.

```bash
xcrun notarytool submit target/release/s3browser.dmg --key AuthKey_XXXX.p8 --key-id XXXX --issuer XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX --wait
```

Rồi ghim kết quả vào file. Bỏ bước này thì máy người dùng phải hỏi Apple mỗi lần
mở, và app sẽ không mở được khi họ offline:

```bash
xcrun stapler staple target/release/s3browser.dmg
xcrun stapler validate target/release/s3browser.dmg
```

Kiểm tra cuối cùng — cái này mô phỏng đúng thứ Gatekeeper làm trên máy người
dùng, và là bước duy nhất thật sự chứng minh việc ký đã đúng:

```bash
spctl --assess --type execute --verbose=4 target/release/s3browser.app
```

## Auto-update

`cargo-packager-updater` cần hai thứ mà repo không tự có: một cặp khoá ký cho
bản cập nhật, và một endpoint host file manifest. Khoá riêng **không được** nằm
trong repo — nếu lộ thì bất kỳ ai cũng ký được một bản "cập nhật" mà app của
người dùng sẽ tự động cài.

Khi nào quyết định host ở đâu thì mới làm bước này. Trước đó, viết code updater
là viết cho một endpoint tưởng tượng, không kiểm chứng được gì.

## Kiểm chứng bản dựng trên máy sạch

Ký và notarize xong trên máy đã dựng thì hầu như luôn trông ổn, kể cả khi thật
ra hỏng — máy đó đã tin chứng chỉ sẵn rồi. Muốn biết người dùng thật sự thấy gì
thì phải tải file qua mạng về một máy khác chưa từng có chứng chỉ đó:

```bash
# Thuộc tính quarantine chỉ được gắn khi file đi qua trình duyệt hoặc mạng.
xattr -p com.apple.quarantine ~/Downloads/s3browser.dmg
```

Copy bằng USB hoặc `scp` sẽ không có thuộc tính này, nên bài thử sẽ dễ hơn thực
tế và không nói lên điều gì.

# Dựng bản Windows từ máy macOS

Chạy được, và không cần máy Windows nào. Nhưng phải vá `vendor/gpui` ba chỗ —
xem phần cuối, vì đó là thứ sẽ vỡ lại mỗi lần nâng gpui lên bản mới.

## Đồ nghề

```bash
brew install llvm lld cmake nasm     # clang-cl, llvm-lib, llvm-rc, lld-link
cargo install cargo-xwin
rustup target add x86_64-pc-windows-msvc
```

Chú ý `lld` là formula **riêng**, không nằm trong `llvm`. Thiếu nó thì
`cargo-xwin` báo lỗi linker chứ không báo thiếu gói, mất một vòng dò.

`cmake` và `nasm` là cho `aws-lc-sys` (thư viện mã hoá của AWS SDK) và
`rusqlite` bundled. Cả hai đều biên dịch mã C/asm nên cần trình biên dịch nhắm
được ABI của MSVC — đó là việc của `clang-cl`, `clang` của Apple không làm được.

Lần chạy đầu `cargo-xwin` tải CRT và Windows SDK của Microsoft về
`~/.cache/cargo-xwin`, khoảng 1–2 GB. Cộng với cây `target` cho một target nữa,
nên trừ sẵn **10–15 GB** đĩa trống.

## Lệnh dựng

```bash
export PATH="/opt/homebrew/opt/lld/bin:/opt/homebrew/opt/llvm/bin:$PATH"
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo xwin build --release --target x86_64-pc-windows-msvc -p s3browser
```

Kết quả: `target/x86_64-pc-windows-msvc/release/s3browser.exe`, khoảng 42 MB.

### `crt-static` không phải tuỳ chọn làm màu

Bỏ cờ đó ra thì exe import `VCRUNTIME140.dll`. DLL này **không** đi kèm Windows;
nó tới từ gói Visual C++ Redistributable. Máy nào cũng thường có sẵn vì phần mềm
khác kéo về, nhưng "thường có" không phải "chắc có", và máy thiếu thì người dùng
gặp đúng cái hộp thoại thiếu-DLL — thứ gần như không ai tự sửa được. Với app
phân phối trực tiếp, đổi vài MB lấy việc bỏ hẳn một phụ thuộc ngoài là đáng.

Kiểm lại bằng cách liệt kê DLL mà exe cần:

```bash
llvm-readobj --coff-imports target/x86_64-pc-windows-msvc/release/s3browser.exe \
  | grep "Name:" | sed 's/.*Name: //' | sort -u
```

Danh sách đúng thì mọi dòng đều là DLL hệ thống: `d3d11`, `dxgi`, `dcomp`,
`dwrite`, `d3dcompiler_47`, `icuuc`, `comctl32`… Thấy `VCRUNTIME140.dll` hoặc
`api-ms-win-crt-*` là cờ `crt-static` chưa ăn.

## Ba chỗ đã vá trong `vendor/gpui`

Gốc chung của cả ba: build script chạy trên **host**, nên `cfg(target_os =
"windows")` trong `build.rs` là *sai* khi cross-compile — nó hỏi "máy đang dựng
có phải Windows không", trong khi thứ cần hỏi là "đang dựng *cho* Windows".
gpui gốc chỉ dựng cho Windows *trên* Windows nên chưa bao giờ lộ ra.

**1. Shader không còn đọc theo đường dẫn máy build.** Đường debug của
`directx_renderer.rs` biên dịch HLSL lúc chạy bằng `D3DCompileFromFile`, với
đường dẫn ghép từ `env!("CARGO_MANIFEST_DIR")`. Exe mang sang máy khác là đi tìm
`/Users/…/vendor/gpui/src/platform/windows/shaders.hlsl` — không có, renderer
chết ngay lúc khởi tạo. Còn đường release thì `include!` file bytecode do
`fxc.exe` sinh ra, mà `fxc.exe` là chương trình Windows nên cross-compile không
chạy được.

Nay `build_shader_blob` nhúng thẳng HLSL bằng `include_str!` rồi gọi
`D3DCompile`. Đổi lại mất trình xử lý `#include` của hệ tệp, nên một dòng
`#include "alpha_correction.hlsl"` được ghép tay — `D3DCompile` không có thư mục
nào để tra.

`build.rs` phát `cargo::rustc-cfg=gpui_embedded_hlsl` khi host không phải
Windows, và renderer dùng đường biên dịch-lúc-chạy khi thấy cfg đó. Dựng **trên**
Windows thì vẫn đi đường `fxc` như gốc, không đổi gì.

Cái giá: biên dịch 16 shader lúc khởi động thay vì nạp bytecode có sẵn. Cờ tối
ưu được chọn theo profile chứ không kế thừa cờ debug, nếu không thì bản release
cross-compile sẽ chạy shader chưa tối ưu mà chẳng có gì báo.

**2. `embed-resource` thôi bị host-gate.** Trong `Cargo.toml` nó khai dưới
`[target.'cfg(target_os = "windows")'.build-dependencies]`. Mà `cfg()` trên
build-dependency xét theo host — nên nó biến mất đúng lúc cross-compile cần tới.

**3. Đường dẫn manifest trong `.rc` phải tuyệt đối.** `embed-resource` tiền xử
lý `.rc` vào `OUT_DIR` rồi mới gọi trình biên dịch tài nguyên, mà trình đó tra
đường dẫn tương đối theo thư mục chứa `.rc` — tới đây là hỏng. Trên Windows
`rc.exe` được đưa thêm thư mục gốc nên không lộ; `llvm-rc` thì báo
`file not found`. `build.rs` nay chép `.rc` sang `OUT_DIR` với đường dẫn tuyệt
đối, và **chỉ làm khi host không phải Windows**: đường dẫn tuyệt đối trên Windows
là `C:\…`, mà `\` trong chuỗi `.rc` là ký tự thoát.

Thiếu bước này exe vẫn dựng được, nhưng mất manifest, tức mất `dpiAwareness
PerMonitorV2`. Hậu quả không phải lỗi mà là app mờ nhoè trên màn HiDPI — và
grep cả `platform/windows` thì không có lời gọi `SetProcessDpiAwarenessContext`
nào, nên manifest là **đường duy nhất** để gpui khai báo DPI awareness.

Kiểm nhanh xem manifest có vào exe không:

```bash
strings -a target/x86_64-pc-windows-msvc/release/s3browser.exe | grep PerMonitorV2
```

## Chưa kiểm chứng

Exe này **chưa từng chạy trên Windows**. Những gì kiểm được từ macOS chỉ là cấu
trúc file: đúng `PE32+ (GUI)`, đúng bảng import, manifest và HLSL nằm trong
binary, không còn đường dẫn của máy build. Việc renderer DirectX có dựng nổi
device thật, glass Acrylic có bật, Credential Manager có nhận khoá hay không —
tất cả đều phải chạy thật trên Windows mới biết.
