# Agent-Hints: Tool Copy + URL-Tolerant s3_key Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Agents holding only an attachment URL call `sheet_read` directly: rewritten tool copy tells them when to use the tools, and `s3_key` accepts a full object-store URL that the server converts to a key.

**Architecture:** All copy changes live in `src/tools.rs` (`#[tool(description)]`, arg doc comments, server `instructions`). URL conversion is one new pure function `resolve_input` in `src/s3keys.rs` that wraps the existing `validate_input_key`; tool call sites swap `validate_input_key` for `resolve_input`. No new crates, no protocol changes, security rules untouched.

**Tech Stack:** Rust (edition 2024), rmcp 3 `#[tool]` macro, plain string splitting for URL parsing (no url crate).

**Spec:** `docs/superpowers/specs/2026-11-06-agent-hints-design.md`

---

## Task 1: `resolve_input` — URL-tolerant key resolution

**Files:**
- Modify: `src/s3keys.rs`
- Test: `src/s3keys.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the bottom of the `mod tests` block in `src/s3keys.rs` (after `output_key_rejects_traversal`):

```rust
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
            resolve_input("http://minio:9000/uploads/workspaces/a/b.xlsx", &cfg)
                .unwrap(),
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
            resolve_input("https://uploads.s3.amazonaws.com/workspaces/a/b.xlsx", &cfg)
                .unwrap(),
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
        let err = resolve_input("http://minio:9000", &cfg).unwrap_err().to_string();
        assert!(err.contains("no object key"), "message: {err}");
    }
```

Note: the existing `s3(&[..])` helper builds an `S3Config` with bucket `b` — reuse it for the passthrough test (empty allowlist, any bucket), but the URL tests need bucket `uploads`, so `url_s3` constructs the struct directly as shown above.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test s3keys 2>&1 | grep -E "error|cannot find" | head -5`
Expected: compile error `cannot find function resolve_input in this scope` (tests reference an undefined function).

- [ ] **Step 3: Implement `resolve_input`**

Add to `src/s3keys.rs` after `validate_input_key` (before `normalize_output_key`):

```rust
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
            )))
        }
    };
    // Virtual-host style: host is "<bucket>.<endpoint-domain>".
    if let Some(domain) = host.strip_prefix(&format!("{}.", s3.bucket))
        && !domain.is_empty()
    {
        return validate_input_key(path, s3);
    }
    // Path style: first path segment is the bucket.
    let key = match path.strip_prefix(&format!("{}/", s3.bucket)) {
        Some(key) => key,
        None => {
            return Err(InvalidKey(format!(
                "URL host or first path segment does not match bucket '{}' (got '{raw}'); expected http://<endpoint>/{}/<key>",
                s3.bucket, s3.bucket
            )))
        }
    };
    validate_input_key(key, s3)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test s3keys 2>&1 | tail -3`
Expected: `test result: ok` with all prior s3keys tests plus the 7 new ones passing (13 total in module).

- [ ] **Step 5: Commit**

```bash
git add src/s3keys.rs
git commit -m "feat: resolve_input accepts object-store URLs, converts to S3 key"
```

---

## Task 2: Wire `resolve_input` into `sheet_read` and `sheet_apply`

**Files:**
- Modify: `src/tools.rs:146-147` (`sheet_read`), `src/tools.rs:186-187` (`sheet_apply`)
- Test: `src/tools.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/tools.rs` (after `sheet_read_rejects_dangerous_key`):

```rust
    /// S3 config for tools-level URL tests: unreachable endpoint so any
    /// network attempt fails AFTER key validation, proving the URL was
    /// accepted and converted.
    fn url_cfg() -> Config {
        Config {
            host: "127.0.0.1".into(),
            port: 0,
            auth_keys: vec![],
            s3: Some(crate::config::S3Config {
                bucket: "uploads".into(),
                region: "us-east-1".into(),
                access_key_id: "ak".into(),
                secret_access_key: "sk".into(),
                endpoint_url: "http://127.0.0.1:1".into(),
                use_path_style: true,
                allowed_key_prefixes: vec![],
            }),
            output_prefix: "excel-mcp-out/".into(),
            inline_max_bytes: 1024,
            max_file_bytes: 1024,
        }
    }

    #[tokio::test]
    async fn sheet_read_accepts_url_and_passes_validation() {
        // The URL converts to key "workspaces/a.xlsx"; validation passes
        // (no allowlist configured); the S3 fetch itself fails because the
        // endpoint is unreachable — but with a fetch error, NOT a key error.
        let server = ExcelMcp::new(url_cfg());
        let result = server
            .sheet_read(Parameters(SheetReadArgs {
                s3_key: "http://127.0.0.1:1/uploads/workspaces/a.xlsx".into(),
                sheet: None,
                max_rows: 500,
                format: None,
            }))
            .await;
        let msg = match &result {
            Err(e) => e.message.to_string(),
            Ok(r) => extract_text(r).unwrap_or_default(),
        };
        assert!(
            !msg.contains("does not match bucket") && !msg.contains("no object key"),
            "URL should have been accepted, got: {msg}"
        );
    }

    #[tokio::test]
    async fn sheet_read_rejects_url_with_traversal() {
        let server = ExcelMcp::new(url_cfg());
        let result = server
            .sheet_read(Parameters(SheetReadArgs {
                s3_key: "http://127.0.0.1:1/uploads/../etc/passwd".into(),
                sheet: None,
                max_rows: 500,
                format: None,
            }))
            .await;
        let msg = match &result {
            Err(e) => e.message.to_string(),
            Ok(r) => extract_text(r).unwrap_or_default(),
        };
        assert!(
            msg.contains("invalid s3 key"),
            "traversal in URL must be rejected as a key error, got: {msg}"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test tools 2>&1 | grep -E "should have been accepted|must be rejected" | head -5`
Expected: first test FAILS with "URL should have been accepted, got: invalid s3 key: key must be a plain '/'-separated path" — the old `validate_input_key` rejects the URL form directly.

The second test may already pass (old validation rejects URLs with traversal too) — that's fine, it guards the behavior through the swap.

- [ ] **Step 3: Swap call sites to `resolve_input`**

In `src/tools.rs`, `sheet_read` (line ~146):

```rust
        crate::s3keys::resolve_input(&args.s3_key, &s3)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
```

(replacing `crate::s3keys::validate_input_key(&args.s3_key, &s3)`)

In `src/tools.rs`, `sheet_apply` (line ~186):

```rust
        let resolved_key = crate::s3keys::resolve_input(&args.s3_key, &s3)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
```

`sheet_apply` currently ignores the validated return value, so bind it (or ignore with `let _ =` — prefer binding, the fetch below should use it for clarity):

Then update the fetch call in `sheet_apply` (line ~194) from `&args.s3_key` to `&resolved_key`:

```rust
        let tmp = crate::storage::fetch_to_tempfile(store.as_ref(), &resolved_key, max)
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
```

Also update `sheet_read`'s fetch call the same way (line ~152): bind the resolved key and use it:

```rust
        let resolved_key = crate::s3keys::resolve_input(&args.s3_key, &s3)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        // ... client build ...
        let tmp = crate::storage::fetch_to_tempfile(store.as_ref(), &resolved_key, max)
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
```

- [ ] **Step 3b: Audit for other `validate_input_key` callers**

Run: `grep -rn "validate_input_key" src/ tests/`
Expected: only `src/s3keys.rs` (definition + `resolve_input` internals). If integration tests reference it, they don't (they call tools over HTTP).

- [ ] **Step 4: Run the full test suite**

Run: `cargo test 2>&1 | grep -E "^test result" | head -3`
Expected: all suites pass — 46 prior tests + 7 s3keys + 2 tools = 55 passing.

- [ ] **Step 5: Commit**

```bash
git add src/tools.rs
git commit -m "feat: sheet_read/sheet_apply accept object-store URLs via resolve_input"
```

---

## Task 3: Rewritten agent-first tool copy

**Files:**
- Modify: `src/tools.rs` — three `#[tool(description = …)]` attributes, `SheetReadArgs::s3_key` / `SheetApplyArgs::s3_key` doc comments, server `instructions`

No new tests: descriptions are string literals; the integration suite already asserts tool names (Task 4 adds a copy assertion there for the URL behavior, which is the part that can drift).

- [ ] **Step 1: Replace the three descriptions**

`sheet_read`:

```rust
    #[tool(
        description = "Read one sheet of an .xlsx workbook from S3. Use this whenever you need data from an Excel file, including platform attachments whose URL points at the object store: pass the attachment's URL or its S3 key as s3_key (URLs are converted to keys automatically). Do NOT download attachments or parse xlsx files locally. Returns sheet list, cells (A1-keyed values plus formulas), merged ranges and a truncation flag. Values are recomputed with a formula engine when possible, else cached values are returned (values_are_cached=true)."
    )]
```

`sheet_apply`:

```rust
    #[tool(
        description = "Edit an .xlsx workbook from S3 and write a new copy: set cell values/formulas, insert rows/columns, add or rename sheets. The source object is never modified. Pass the source workbook's URL or S3 key as s3_key (URLs are converted to keys automatically); output_key must be an S3 key ending in .xlsx."
    )]
```

`create_xlsx`:

```rust
    #[tool(
        description = "Create a new Excel (.xlsx) workbook from data: a 2-D array of cell values, or {\"sheets\":[{\"name\",\"rows\"}]} for several sheets. Strings starting with \"=\" are formulas. Prefer this over emitting CSV/markdown tables when the deliverable is a real spreadsheet. Returns the output s3_key and the file bytes base64-encoded when small."
    )]
```

- [ ] **Step 2: Update the arg doc comments**

`SheetReadArgs`:

```rust
    /// S3 key of the workbook to read, or the full object-store URL of an
    /// attachment (converted to a key automatically).
    pub s3_key: String,
```

`SheetApplyArgs`:

```rust
    /// S3 key of the source workbook, or its full object-store URL
    /// (converted to a key automatically). Never modified.
    pub s3_key: String,
```

- [ ] **Step 3: Update server instructions**

Replace the `.with_instructions(...)` body in `get_info`:

```rust
            .with_instructions(
                "Excel workbook tools over S3 — use them instead of parsing xlsx files yourself. \
                 sheet_read returns cells, formulas and recomputed values for one sheet; \
                 sheet_apply edits an existing workbook and writes a new copy; \
                 create_xlsx builds a new .xlsx from data. All inputs are S3 keys, but full \
                 object-store URLs (e.g. platform attachment URLs) are accepted and converted \
                 automatically — do not download attachments to parse them locally.",
            )
```

- [ ] **Step 4: Build and run tests**

Run: `cargo test 2>&1 | grep -E "^test result" | head -3`
Expected: same counts as Task 2 (55 passing) — copy changes don't affect behavior.

- [ ] **Step 5: Commit**

```bash
git add src/tools.rs
git commit -m "feat: agent-first tool descriptions and instructions"
```

---

## Task 4: Integration test — URL round trip

**Files:**
- Modify: `tests/integration.rs` (new test after `seeded_external_object_reads_back`)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
#[ignore = "needs Docker + integration feature"]
fn seeded_object_reads_back_via_url() {
    let stack = Stack::start().expect("stack");
    let sheets = excel_fixture::fixture_bytes();
    stack.seed_object("workspaces/ws-9/att.xlsx", sheets).unwrap();
    // The URL form a platform agent would hold: endpoint + bucket + key.
    let url = format!("{}/uploads/workspaces/ws-9/att.xlsx", stack.s3_endpoint);
    let read = stack
        .tool_result_json("sheet_read", serde_json::json!({ "s3_key": url }), 40)
        .unwrap();
    assert_eq!(read["cells"]["A1"]["value"].as_str().unwrap(), "seeded");
}
```

- [ ] **Step 2: Run it to verify it fails (or passes)**

Run: `cargo test --features integration seeded_object_reads_back_via_url -- --ignored 2>&1 | tail -3`

If the stack fails to start (Docker unavailable), note it and move on — this test is excluded by default and runs in CI. If it runs: it must PASS (Task 2 already wired the behavior); the test exists to lock the contract. If it fails, Task 2's wiring is broken — debug before proceeding.

- [ ] **Step 3: Commit**

```bash
git add tests/integration.rs
git commit -m "test: integration case for URL-form s3_key round trip"
```

---

## Task 5: Live verification

**Files:** none (runtime check)

- [ ] **Step 1: Start the server locally**

Requires a reachable MinIO (or skip this task with a note). Check for a local MinIO:

Run: `docker ps --filter name=minio --format '{{.Names}}' 2>/dev/null || true`
If empty and no local S3 is available, skip with: "live check skipped — no local MinIO; integration tests cover the path".

Otherwise:

```bash
MCP_HOST=127.0.0.1 MCP_PORT=18099 \
S3_BUCKET=uploads AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin \
AWS_ENDPOINT_URL=http://localhost:9000 S3_USE_PATH_STYLE=true \
target/debug/excel-mcp
```

- [ ] **Step 2: Verify descriptions and URL acceptance over HTTP**

```bash
# initialize + tools/list: confirm descriptions mention URLs
curl -s http://127.0.0.1:18099/mcp -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"verify","version":"1"}}}' \
  -D headers.txt > init.json
SESSION=$(grep -i mcp-session-id headers.txt | awk '{print $2}' | tr -d '\r')
curl -s http://127.0.0.1:18099/mcp -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -H "Mcp-Session-Id: $SESSION" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | grep -o 'converted to keys automatically' | head -1
```

Expected: the grep finds the phrase in `sheet_read`'s description.

Then one `sheet_read` call with a URL form pointing at a seeded object (adjust key/URL to whatever the local MinIO holds):

```bash
curl -s http://127.0.0.1:18099/mcp -H "Mcp-Session-Id: $SESSION" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"sheet_read","arguments":{"s3_key":"http://localhost:9000/uploads/excel-mcp-out/<uuid>.xlsx"}}}'
```

Expected: a normal read result (`{"sheet":...,"cells":...}`) or a NotFound-style error — anything EXCEPT a key-validation error, which would mean the URL was not converted.

- [ ] **Step 3: Stop the server, note results**

Kill the background server. Record in the final report which checks ran and what they showed.

---

## Task 6: README + version bump

**Files:**
- Modify: `README.md` (EN `sheet_read` bullet + 中文「Agent 侧接入」段)
- Modify: `Cargo.toml:3`, `Cargo.lock`

- [ ] **Step 1: Update README EN tool bullets**

In the Three tools list, extend the `sheet_read` bullet:

```markdown
- **`sheet_read`** — read one sheet: cells (values + formulas), merged
  ranges, sheet list. Values are recomputed with a formula engine
  ([IronCalc](https://ironcalc.com/)) when possible; cached values
  (flagged `values_are_cached: true`) otherwise. **`s3_key` also accepts
  the full object-store URL of an attachment** (e.g. a platform
  attachment's `url` field) — the server converts it to the key, so
  agents don't need to download or parse files locally.
```

And add the same note to the `sheet_apply` bullet's first sentence:
"Pass the source workbook's URL or S3 key as `s3_key` (URLs converted automatically)."

- [ ] **Step 2: Update README 中文 section**

In `### Agent 侧接入`, after the JSON block, extend the closing paragraph:

```markdown
接入后即可调用 `create_xlsx` / `sheet_read` / `sheet_apply` 三个工具。
`s3_key` 既接受 S3 key，也接受附件的完整对象存储 URL（如平台附件的
`url` 字段，形如 `http://minio:9000/uploads/workspaces/.../file.xlsx`），
服务端会自动去掉 endpoint 和 bucket 段转换成 key——agent 不需要下载
附件到本地解析。参数示例见上文英文部分的 Example calls。
```

- [ ] **Step 3: Bump version**

`Cargo.toml`: `version = "0.1.1"` → `version = "0.1.2"`.
Then run `cargo check` to refresh `Cargo.lock`, and update the README `0.1.0` image tag references if the k8s example uses them (check: `grep -n "0.1.0" README.md` — update matching image tags to `0.1.2`).

- [ ] **Step 4: Final full test run**

Run: `cargo test 2>&1 | grep -E "^test result" | head -3`
Expected: 55 passing, 0 failed.

- [ ] **Step 5: Commit**

```bash
git add README.md Cargo.toml Cargo.lock
git commit -m "docs+chore: document URL-tolerant s3_key, bump 0.1.2"
```
