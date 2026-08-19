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
