//! S3 key validation: the security boundary every tool input passes first.
//! Port of markitdown-mcp's `validate_s3_key` semantics.

use crate::config::S3Config;

/// Extensions never worth fetching: cheap block on smuggling executables.
const DANGEROUS_EXTENSIONS: [&str; 12] = [
    ".exe", ".bat", ".cmd", ".sh", ".ps1", ".scr", ".vbs", ".jar", ".app", ".dmg", ".pkg", ".deb",
];

#[derive(Debug, thiserror::Error)]
#[error("invalid s3 key: {0}")]
pub struct InvalidKey(String);

/// Validate an input key against the configured allowlist.
///
/// Rules (markitdown-mcp parity):
/// - non-empty, no leading `/`, no `//`, no `.`/`..` segments, no backslash
/// - extension (if any) not in DANGEROUS_EXTENSIONS
/// - when `S3_KEY_PREFIXES` is set, key must start with one prefix at a
///   segment boundary (`workspaces/ws-1` matches `workspaces/ws-1/a.xlsx`
///   and `workspaces/ws-1`, not `workspaces/ws-11/a.xlsx`)
pub fn validate_input_key(key: &str, s3: &S3Config) -> Result<String, InvalidKey> {
    let key = key.trim();
    if key.is_empty() {
        return Err(InvalidKey("key is empty".into()));
    }
    if key.starts_with('/') {
        return Err(InvalidKey("key must not start with '/'".into()));
    }
    if key.contains('\\') || key.contains("//") {
        return Err(InvalidKey("key must be a plain '/'-separated path".into()));
    }
    if key
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(InvalidKey(
            "key must not contain '.', '..' or empty segments".into(),
        ));
    }
    let lower = key.to_lowercase();
    if DANGEROUS_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
        return Err(InvalidKey("key extension is refused".into()));
    }
    if !s3.allowed_key_prefixes.is_empty()
        && !s3.allowed_key_prefixes.iter().any(|p| {
            key == p.as_str()
                || key.starts_with(p.as_str())
                    && (p.ends_with('/') || key.as_bytes().get(p.len()) == Some(&b'/'))
        })
    {
        return Err(InvalidKey("key is outside the configured allowlist".into()));
    }
    Ok(key.to_string())
}

/// Resolve a tool's input key argument: a plain S3 key passes through; a
/// full object-store URL (as found in platform attachment metadata) is
/// converted to its key first. Either way the result is then validated by
/// `validate_input_key`, so the security rules are identical for both
/// forms.
pub fn resolve_input(raw: &str, s3: &S3Config) -> Result<String, InvalidKey> {
    let raw = raw.trim();
    let Some(rest) = raw
        .strip_prefix("http://")
        .or_else(|| raw.strip_prefix("https://"))
    else {
        // Not a URL: plain key, validated as-is.
        return validate_input_key(raw, s3);
    };
    // Drop the query string (presign signatures) before splitting.
    let rest = rest.split('?').next().unwrap_or(rest);
    let (host, path) = match rest.split_once('/') {
        Some(split) => split,
        None => {
            return Err(InvalidKey(format!(
                "URL contains no object key (got '{raw}'); pass 's3_key' as a key or a URL like http://<endpoint>/{}/<key>",
                s3.bucket
            )));
        }
    };
    // Virtual-host style: host is "<bucket>.<endpoint-domain>".
    if let Some(domain) = host.strip_prefix(&format!("{}.", s3.bucket))
        && !domain.is_empty()
    {
        return validate_input_key(path, s3);
    }
    // Bucket-only or trailing-slash URL: segment matches, but no key.
    if path == s3.bucket || path == format!("{}/", s3.bucket) {
        return Err(InvalidKey(format!(
            "URL contains no object key (got '{raw}'); pass 's3_key' as a key or a URL like http://<endpoint>/{}/<key>",
            s3.bucket
        )));
    }
    // Path style: first path segment is the bucket.
    let key = match path.strip_prefix(&format!("{}/", s3.bucket)) {
        Some(key) => key,
        None => {
            return Err(InvalidKey(format!(
                "URL host or first path segment does not match bucket '{}' (got '{raw}'); expected http://<endpoint>/{}/<key>",
                s3.bucket, s3.bucket
            )));
        }
    };
    validate_input_key(key, s3)
}

/// Normalize an output key: validation minus the input allowlist (outputs
/// go to `S3_OUTPUT_PREFIX`, not the input roots) plus the .xlsx suffix
/// rule.
pub fn normalize_output_key(key: &str) -> Result<String, InvalidKey> {
    let key = key.trim();
    if key.is_empty() {
        return Err(InvalidKey("key is empty".into()));
    }
    if key.starts_with('/')
        || key.contains('\\')
        || key.contains("//")
        || key
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(InvalidKey("key must be a plain '/'-separated path".into()));
    }
    let lower = key.to_lowercase();
    if DANGEROUS_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
        return Err(InvalidKey("key extension is refused".into()));
    }
    if !lower.ends_with(".xlsx") {
        return Err(InvalidKey("output key must end with .xlsx".into()));
    }
    Ok(key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3(prefixes: &[&str]) -> S3Config {
        S3Config {
            bucket: "b".into(),
            region: "r".into(),
            access_key_id: "ak".into(),
            secret_access_key: "sk".into(),
            endpoint_url: "http://minio:9000".into(),
            use_path_style: true,
            allowed_key_prefixes: prefixes.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn rejects_empty_and_absolute() {
        assert!(validate_input_key("", &s3(&[])).is_err());
        assert!(validate_input_key("  ", &s3(&[])).is_err());
        assert!(validate_input_key("/abs/path.xlsx", &s3(&[])).is_err());
    }

    #[test]
    fn rejects_traversal_and_double_slash() {
        assert!(validate_input_key("a/../b.xlsx", &s3(&[])).is_err());
        assert!(validate_input_key("a//b.xlsx", &s3(&[])).is_err());
        assert!(validate_input_key("a/./b.xlsx", &s3(&[])).is_err());
        assert!(validate_input_key(r"a\b.xlsx", &s3(&[])).is_err());
    }

    #[test]
    fn rejects_dangerous_extensions() {
        assert!(validate_input_key("payload.exe", &s3(&[])).is_err());
        assert!(validate_input_key("payload.CMD", &s3(&[])).is_err());
    }

    #[test]
    fn allowlist_matches_segment_aware() {
        let cfg = s3(&["workspaces/ws-1"]);
        assert!(validate_input_key("workspaces/ws-1", &cfg).is_ok());
        assert!(validate_input_key("workspaces/ws-1/a.xlsx", &cfg).is_ok());
        assert!(validate_input_key("workspaces/ws-11/a.xlsx", &cfg).is_err());
        assert!(validate_input_key("other/a.xlsx", &cfg).is_err());
    }

    #[test]
    fn allowlist_prefix_with_trailing_slash() {
        let cfg = s3(&["workspaces/"]);
        assert!(validate_input_key("workspaces/a.xlsx", &cfg).is_ok());
        assert!(validate_input_key("other/a.xlsx", &cfg).is_err());
    }

    #[test]
    fn no_allowlist_accepts_any_valid_key() {
        let cfg = s3(&[]);
        assert!(validate_input_key("any/where/a.xlsx", &cfg).is_ok());
    }

    #[test]
    fn output_key_requires_xlsx_suffix() {
        assert!(normalize_output_key("out/a.xlsx").is_ok());
        assert!(normalize_output_key("out/a.csv").is_err());
        assert!(normalize_output_key("out/a.XLSX").is_ok());
    }

    #[test]
    fn output_key_rejects_traversal() {
        assert!(normalize_output_key("out/../etc/passwd.xlsx").is_err());
        assert!(normalize_output_key("/abs.xlsx").is_err());
    }

    // --- resolve_input: URL forms ---

    fn url_s3() -> S3Config {
        S3Config {
            bucket: "uploads".into(),
            region: "r".into(),
            access_key_id: "ak".into(),
            secret_access_key: "sk".into(),
            endpoint_url: "http://minio:9000".into(),
            use_path_style: true,
            allowed_key_prefixes: vec!["workspaces/".into()],
        }
    }

    #[test]
    fn resolve_plain_key_passthrough() {
        let cfg = s3(&[]);
        assert_eq!(
            resolve_input("workspaces/a/b.xlsx", &cfg).unwrap(),
            "workspaces/a/b.xlsx"
        );
    }

    #[test]
    fn resolve_path_style_url() {
        // MinIO in-cluster form: endpoint + bucket + key in the path.
        let cfg = url_s3();
        assert_eq!(
            resolve_input("http://minio:9000/uploads/workspaces/a/b.xlsx", &cfg).unwrap(),
            "workspaces/a/b.xlsx"
        );
    }

    #[test]
    fn resolve_path_style_url_with_query() {
        // Presigned URLs carry the signature in the query string.
        let cfg = url_s3();
        assert_eq!(
            resolve_input(
                "http://minio:9000/uploads/workspaces/a/b.xlsx?X-Amz-Signature=abc&X-Amz-Expires=600",
                &cfg
            )
            .unwrap(),
            "workspaces/a/b.xlsx"
        );
    }

    #[test]
    fn resolve_virtual_host_style_url() {
        // bucket.endpoint host: path starts directly at the key.
        let cfg = url_s3();
        assert_eq!(
            resolve_input("https://uploads.s3.amazonaws.com/workspaces/a/b.xlsx", &cfg).unwrap(),
            "workspaces/a/b.xlsx"
        );
    }

    #[test]
    fn resolve_url_still_enforces_key_rules() {
        // Traversal smuggled in the URL path is caught by validate_input_key.
        let cfg = url_s3();
        assert!(resolve_input("http://minio:9000/uploads/../etc/passwd", &cfg).is_err());
    }

    #[test]
    fn resolve_virtual_host_url_still_enforces_key_rules() {
        // A traversal smuggled in the virtual-host URL path must be caught
        // by validate_input_key, same as the path-style branch.
        let cfg = url_s3();
        assert!(resolve_input("https://uploads.s3.amazonaws.com/../etc/passwd", &cfg).is_err());
    }

    #[test]
    fn resolve_bucket_only_url_has_no_key() {
        let cfg = url_s3();
        for url in ["http://minio:9000/uploads", "http://minio:9000/uploads/"] {
            let err = resolve_input(url, &cfg).unwrap_err().to_string();
            assert!(err.contains("no object key"), "for {url} got: {err}");
        }
    }

    #[test]
    fn resolve_url_first_segment_not_bucket() {
        // A URL pointing elsewhere is rejected with an actionable message.
        let cfg = url_s3();
        let err = resolve_input("http://other-host:9000/otherbucket/workspaces/a.xlsx", &cfg)
            .unwrap_err()
            .to_string();
        assert!(err.contains("uploads"), "message names the bucket: {err}");
    }

    #[test]
    fn resolve_bare_host_url_has_no_key() {
        let cfg = url_s3();
        let err = resolve_input("http://minio:9000", &cfg)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no object key"), "message: {err}");
    }
}
