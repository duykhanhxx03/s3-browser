//! Errors, in words the user can act on.
//!
//! What was here before was a raw provider message in red at the bottom of the
//! window, replaced by the next thing that happened. Two problems with that,
//! and they compound: the message is written for whoever wrote the SDK, and it
//! is gone before it can be read twice.
//!
//! So a failure keeps three things apart. The **summary** says what happened in
//! ordinary words. The **detail** is the provider's own text, kept verbatim
//! because that is what gets pasted into a support ticket. The **fix** is the
//! button, and only exists where there is genuinely one thing to do — a button
//! that merely restates the problem is worse than no button.

use gpui::SharedString;

/// What can be done about a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fix {
    /// The token cannot list buckets, but reaching one by name still works.
    ///
    /// Not really an error at all: a bucket-scoped token is the setup R2's own
    /// documentation recommends, so this is the normal path for a lot of people
    /// and the app should hand them the door rather than the diagnosis.
    OpenBucketByName,
    /// The credentials are wrong or expired. Nothing about the request will
    /// help; the profile has to change.
    EditProfile,
    /// The request was fine and did not arrive, or was throttled.
    Retry,
}

impl Fix {
    pub fn label(self) -> &'static str {
        match self {
            Fix::OpenBucketByName => "Mở bucket theo tên",
            Fix::EditProfile => "Sửa profile",
            Fix::Retry => "Thử lại",
        }
    }
}

/// One thing that went wrong, kept until it is dismissed.
#[derive(Clone, Debug)]
pub struct Failure {
    pub summary: SharedString,
    /// The provider's own message. Never thrown away, however friendly the
    /// summary is: when the summary turns out to be wrong about what happened,
    /// this is the only thing left to go on.
    pub detail: SharedString,
    pub fix: Option<Fix>,
    pub at: i64,
}

impl Failure {
    /// Reads a raw error and works out what to say about it.
    pub fn new(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let (summary, fix) = classify(&raw);
        Self {
            summary: summary.unwrap_or_else(|| first_line(&raw).to_string()).into(),
            detail: raw.into(),
            fix,
            at: s3core::now_epoch(),
        }
    }

    /// Offers a fix only where classification did not find a better one.
    ///
    /// For a workaround that holds whatever the cause — opening a bucket by
    /// name works whether listing was forbidden, misconfigured or unreachable —
    /// while still letting "your key is wrong" win, because that one names the
    /// actual cause and this one only routes around it.
    pub fn or_fix(mut self, fix: Fix) -> Self {
        self.fix = self.fix.or(Some(fix));
        self
    }

    /// A failure whose meaning the caller already knows, because it knows which
    /// request it made. `classify` only sees the text and cannot.
    pub fn known(summary: &str, detail: impl Into<String>, fix: Option<Fix>) -> Self {
        Self {
            summary: summary.to_string().into(),
            detail: detail.into().into(),
            fix,
            at: s3core::now_epoch(),
        }
    }
}

/// The summary is the first line: SDK errors carry a chain of `source:` causes
/// that runs for paragraphs, and a status bar can hold one line.
fn first_line(raw: &str) -> &str {
    raw.lines().next().unwrap_or(raw).trim()
}

/// Recognises the error families worth naming.
///
/// Returns `None` for the summary when nothing matches, so the caller shows the
/// provider's own words rather than a guess. Inventing a friendly summary for
/// an error nobody has classified is how a UI ends up confidently wrong.
fn classify(raw: &str) -> (Option<String>, Option<Fix>) {
    let has = |needle: &str| raw.contains(needle);

    // Credentials first. These also produce `AccessDenied` on some providers,
    // and "your key is wrong" is a more useful thing to hear than "you are not
    // allowed", so they have to be checked before the permission cases.
    if has("InvalidAccessKeyId") || has("SignatureDoesNotMatch") {
        return (
            Some("Khoá truy cập không đúng".into()),
            Some(Fix::EditProfile),
        );
    }
    if has("ExpiredToken") || has("TokenRefreshRequired") || has("InvalidToken") {
        return (
            Some("Phiên đăng nhập đã hết hạn".into()),
            Some(Fix::EditProfile),
        );
    }

    // A token allowed on one bucket and not on ListBuckets is the common R2
    // setup, not a mistake — so this one gets a door rather than a diagnosis.
    if (has("AccessDenied") || has("Forbidden")) && has("ListBuckets") {
        return (
            Some("Token không có quyền liệt kê bucket".into()),
            Some(Fix::OpenBucketByName),
        );
    }
    if has("AccessDenied") || has("Forbidden") {
        // No fix: which permission is missing depends on the request, and this
        // function only has the text. Guessing a button here would send people
        // to edit a profile that is perfectly correct.
        return (Some("Token không có quyền cho thao tác này".into()), None);
    }

    if has("NoSuchBucket") {
        return (Some("Bucket không tồn tại".into()), None);
    }
    if has("NoSuchKey") {
        return (Some("Object không còn ở đó".into()), None);
    }
    if has("BucketAlreadyExists") || has("BucketAlreadyOwnedByYou") {
        return (Some("Tên bucket đã có người dùng".into()), None);
    }
    if has("AccessControlListNotSupported") {
        return (Some("Bucket này đã tắt ACL".into()), None);
    }
    if has("NotImplemented") || has("MethodNotAllowed") {
        return (Some("Provider không hỗ trợ thao tác này".into()), None);
    }

    // Throttling is temporary by definition, so retrying is exactly right.
    if has("SlowDown") || has("ServiceUnavailable") || has("RequestLimitExceeded") {
        return (
            Some("Provider đang chặn bớt yêu cầu".into()),
            Some(Fix::Retry),
        );
    }

    // Transport. `dispatch failure` is what the AWS SDK wraps every one of
    // these in, and it is the string a user is least able to interpret.
    if has("dispatch failure")
        || has("timed out")
        || has("Timeout")
        || has("ConnectionRefused")
        || has("Connection refused")
        || has("dns error")
    {
        return (
            Some("Không kết nối được tới endpoint".into()),
            Some(Fix::Retry),
        );
    }

    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrong_key_is_told_apart_from_a_missing_permission() {
        // Both come back as a 403 from some providers, and they call for
        // opposite actions: one means fix the profile, the other means the
        // profile is fine and the policy is not.
        let (summary, fix) = classify("InvalidAccessKeyId: The key is not valid");
        assert_eq!(summary.as_deref(), Some("Khoá truy cập không đúng"));
        assert_eq!(fix, Some(Fix::EditProfile));

        let (_, fix) = classify("AccessDenied: not authorized to PutObject");
        // Deliberately no button: which permission is missing depends on the
        // request, and offering "sửa profile" would send someone to edit
        // credentials that are perfectly correct.
        assert_eq!(fix, None);
    }

    #[test]
    fn a_bucket_scoped_token_gets_a_door_not_a_diagnosis() {
        // The R2 case, and the reason this whole module exists. A token scoped
        // to one bucket is what R2's own docs recommend, so being unable to
        // list buckets is the normal path for a lot of people.
        let (summary, fix) = classify("AccessDenied when calling ListBuckets");
        assert_eq!(summary.as_deref(), Some("Token không có quyền liệt kê bucket"));
        assert_eq!(fix, Some(Fix::OpenBucketByName));
    }

    #[test]
    fn only_the_temporary_failures_offer_a_retry() {
        // Retrying a throttle or a dropped connection is the right move.
        assert_eq!(classify("SlowDown").1, Some(Fix::Retry));
        assert_eq!(classify("dispatch failure: io error").1, Some(Fix::Retry));

        // Retrying a bucket that does not exist just fails again, more slowly.
        assert_eq!(classify("NoSuchBucket").1, None);
        assert_eq!(classify("NotImplemented").1, None);
    }

    #[test]
    fn an_unrecognised_error_keeps_the_providers_own_words() {
        // The alternative is inventing a friendly summary for something nobody
        // has classified, which is how a UI ends up confidently wrong about
        // what went wrong.
        let (summary, fix) = classify("QuotaExceededForThisTuesday");
        assert_eq!(summary, None);
        assert_eq!(fix, None);

        let failure = Failure::new("QuotaExceededForThisTuesday");
        assert_eq!(failure.summary, "QuotaExceededForThisTuesday");
    }

    #[test]
    fn a_fallback_fix_never_displaces_a_diagnosed_one() {
        // Naming the request lets the classifier reach the R2 case.
        let listing = Failure::new("ListBuckets: AccessDenied").or_fix(Fix::OpenBucketByName);
        assert_eq!(listing.fix, Some(Fix::OpenBucketByName));

        // But the same call site also sees expired credentials and dead
        // networks, and "sửa profile" beats a workaround that will fail the
        // same way. Claiming a permission problem here would send someone to
        // read an IAM policy that is perfectly correct.
        let bad_key = Failure::new("ListBuckets: SignatureDoesNotMatch")
            .or_fix(Fix::OpenBucketByName);
        assert_eq!(bad_key.fix, Some(Fix::EditProfile));
        assert_eq!(bad_key.summary, "Khoá truy cập không đúng");

        let offline = Failure::new("ListBuckets: dispatch failure").or_fix(Fix::OpenBucketByName);
        assert_eq!(offline.fix, Some(Fix::Retry));
    }

    #[test]
    fn the_summary_is_one_line_but_the_detail_is_all_of_it() {
        // SDK errors are a chain of `source:` causes running for paragraphs.
        // The status bar has one line; the panel has the rest, and the rest is
        // what gets pasted into a support ticket.
        let raw = "ServiceError\n  source: dispatch failure\n  source: io error";
        let failure = Failure::new(raw);

        assert_eq!(failure.summary, "Không kết nối được tới endpoint");
        assert_eq!(failure.detail, raw, "the original is never thrown away");

        let unknown = Failure::new("Weird\n  source: also weird");
        assert_eq!(unknown.summary, "Weird", "one line in the bar");
        assert!(unknown.detail.contains("also weird"), "all of it in the panel");
    }
}
