//! Full-stack integration tests against a real MinIO container and the
//! real server binary. Ignored by default: needs Docker. Run with
//! `cargo test --features integration -- --ignored`.

#![cfg(feature = "integration")]

/// The running stack: MinIO container + server child process. Both die
/// with the struct, keeping ports alive for the test body's duration.
struct Stack {
    mcp_base: String, // http://127.0.0.1:<port>/mcp
    _child: std::process::Child,
    // The container is removed synchronously in Drop (docker rm -f);
    // ContainerAsync's own Drop needs a live reactor, so it is forgotten.
    _minio: std::mem::ManuallyDrop<
        testcontainers::ContainerAsync<testcontainers_modules::minio::MinIO>,
    >,
    s3_endpoint: String,
}

impl Stack {
    fn start() -> anyhow::Result<Self> {
        use testcontainers::runners::AsyncRunner;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let minio = runtime.block_on(async {
            testcontainers_modules::minio::MinIO::default()
                .start()
                .await
        })?;
        // From here on the container exists: every error path must remove
        // it via Docker (its own Drop needs a tokio reactor we don't keep)
        // and then forget it, or the drop panics.
        let minio = std::mem::ManuallyDrop::new(minio);
        let cleanup = |m: &std::mem::ManuallyDrop<
            testcontainers::ContainerAsync<testcontainers_modules::minio::MinIO>,
        >| {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", m.id()])
                .output();
        };

        let host_port = match runtime.block_on(minio.get_host_port_ipv4(9000)) {
            Ok(p) => p,
            Err(e) => {
                cleanup(&minio);
                return Err(e.into());
            }
        };
        let s3_endpoint = format!("http://127.0.0.1:{host_port}");

        // Create the uploads bucket via the MinIO Client that ships in
        // the image (object_store has no bucket-creation API). The
        // built-in "local" alias needs credentials configured first.
        let exec_mc = |args: &[&str]| -> std::io::Result<std::process::Output> {
            std::process::Command::new("docker")
                .arg("exec")
                .arg(minio.id())
                .arg("mc")
                .args(args)
                .output()
        };
        let alias = exec_mc(&[
            "alias",
            "set",
            "local",
            "http://127.0.0.1:9000",
            "minioadmin",
            "minioadmin",
        ]);
        let mb = exec_mc(&["mb", "--ignore-existing", "local/uploads"]);
        match (alias, mb) {
            (Ok(a), Ok(b)) if a.status.success() && b.status.success() => {}
            (a, b) => {
                let msg = format!(
                    "alias: {:?} mb: {:?}",
                    a.map(|o| String::from_utf8_lossy(&o.stderr).to_string()),
                    b.map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                );
                cleanup(&minio);
                anyhow::bail!("bucket creation failed: {msg}");
            }
        }

        let port = free_port()?;
        // The CARGO_BIN_* env var is only set when running via `cargo
        // test` with the bin built as a dependency; fall back to the
        // compile-time path when the variable is absent.
        let bin = option_env!("CARGO_BIN_EXCEL_EXCEL_MCP")
            .map(String::from)
            .or_else(|| std::env::var("CARGO_BIN_EXCEL_MCP").ok())
            .unwrap_or_else(|| "target/debug/excel-mcp".to_string());
        let child = match std::process::Command::new(bin)
            .env("MCP_HOST", "127.0.0.1")
            .env("MCP_PORT", port.to_string())
            .env("MCP_AUTH_KEY", "it-key")
            .env("S3_BUCKET", "uploads")
            .env("AWS_ACCESS_KEY_ID", "minioadmin")
            .env("AWS_SECRET_ACCESS_KEY", "minioadmin")
            .env("AWS_ENDPOINT_URL", &s3_endpoint)
            .env("S3_USE_PATH_STYLE", "true")
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                cleanup(&minio);
                return Err(e.into());
            }
        };

        let base = format!("http://127.0.0.1:{port}");
        for _ in 0..120 {
            if let Ok(resp) = reqwest::blocking::get(format!("{base}/health"))
                && resp.status().is_success()
            {
                return Ok(Self {
                    mcp_base: format!("{base}/mcp"),
                    _child: child,
                    _minio: minio,
                    s3_endpoint,
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        cleanup(&minio);
        anyhow::bail!("server never became healthy")
    }

    /// One JSON-RPC call: initialize (fresh session each call — simpler
    /// than tracking Mcp-Session-Id), then the requested method.
    fn mcp_call(
        &self,
        method: &str,
        params: serde_json::Value,
        id: i64,
    ) -> anyhow::Result<serde_json::Value> {
        let client = reqwest::blocking::Client::new();
        let init = serde_json::json!({
            "jsonrpc": "2.0", "method": "initialize", "id": id,
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "it", "version": "1"},
            },
        });
        let resp = client
            .post(&self.mcp_base)
            .bearer_auth("it-key")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&init)
            .send()?;
        if !resp.status().is_success() {
            anyhow::bail!("initialize failed: {}", resp.status());
        }
        // A session may be required for subsequent calls; capture it.
        let session = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(String::from);

        let call = serde_json::json!({
            "jsonrpc": "2.0", "method": method, "id": id + 1000, "params": params,
        });
        let mut req = client
            .post(&self.mcp_base)
            .bearer_auth("it-key")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&call);
        if let Some(sid) = &session {
            req = req.header("Mcp-Session-Id", sid);
        }
        let resp = req.send()?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = resp.text()?;
        // SSE responses carry the JSON-RPC answer in `data:` lines; plain
        // JSON responses are the whole body. Unify to one Option<Value>.
        let parsed: Option<serde_json::Value> = if content_type.contains("text/event-stream") {
            body.lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .next_back()
                .and_then(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        } else {
            serde_json::from_str::<serde_json::Value>(&body).ok()
        };
        let Some(parsed) = parsed else {
            anyhow::bail!("unparseable response ({content_type}): {body}");
        };
        if !status.is_success() {
            anyhow::bail!("call {method} failed: {status} body={body}");
        }
        Ok(parsed)
    }

    /// The first Text block of a tools/call result, parsed as JSON.
    fn tool_result_json(
        &self,
        name: &str,
        args: serde_json::Value,
        id: i64,
    ) -> anyhow::Result<serde_json::Value> {
        let resp = self.mcp_call(
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
            id,
        )?;
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no text in tool result: {resp}"))?;
        Ok(serde_json::from_str(text)?)
    }

    /// Put an object into the test bucket (the sheet_read input path).
    fn seed_object(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let cfg = crate_test_s3(&self.s3_endpoint);
        let res: Result<(), anyhow::Error> = runtime.block_on(async {
            let store = excel_s3_client(&cfg).await?;
            use object_store::ObjectStoreExt;
            store
                .put(&object_store::path::Path::from(key), bytes.into())
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("seed put failed: {e}"))
        });
        res
    }

    /// Read an object back from the test bucket.
    fn get_object(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let cfg = crate_test_s3(&self.s3_endpoint);
        let res: Result<Vec<u8>, anyhow::Error> = runtime.block_on(async {
            let store = excel_s3_client(&cfg).await?;
            use object_store::ObjectStoreExt;
            let get = store
                .get(&object_store::path::Path::from(key))
                .await
                .map_err(|e| anyhow::anyhow!("get failed: {e}"))?;
            let bytes = get
                .bytes()
                .await
                .map_err(|e| anyhow::anyhow!("read failed: {e}"))?;
            Ok(bytes.to_vec())
        });
        res
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = self._child.kill();
        let _ = self._child.wait();
        // Remove the container through the Docker CLI (synchronous, no
        // reactor needed). ContainerAsync itself is never dropped: its
        // Drop panics outside a tokio context, and the ManuallyDrop
        // wrapper means letting the field drop simply does nothing.
        let id = self._minio.id().to_string();
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &id])
            .output();
    }
}

/// Minimal S3Config for in-test object_store clients (bucket creation,
/// seeding, verification). Mirrors the server's env settings.
fn crate_test_s3(endpoint: &str) -> excel_s3_test::S3Config {
    excel_s3_test::S3Config {
        bucket: "uploads".into(),
        region: "us-east-1".into(),
        access_key_id: "minioadmin".into(),
        secret_access_key: "minioadmin".into(),
        endpoint_url: endpoint.into(),
        use_path_style: true,
        allowed_key_prefixes: vec![],
    }
}

/// object_store client for test-side bucket management.
async fn excel_s3_client(
    cfg: &excel_s3_test::S3Config,
) -> anyhow::Result<object_store::aws::AmazonS3> {
    let builder = object_store::aws::AmazonS3Builder::new()
        .with_bucket_name(&cfg.bucket)
        .with_region(&cfg.region)
        .with_access_key_id(&cfg.access_key_id)
        .with_secret_access_key(&cfg.secret_access_key)
        .with_endpoint(&cfg.endpoint_url)
        .with_allow_http(true)
        .with_virtual_hosted_style_request(!cfg.use_path_style);
    Ok(builder.build()?)
}

/// A compile-time-visible holder so the test crate can reach the binary
/// crate's S3Config type without a library target: re-declared locally.
/// `allowed_key_prefixes` exists only for shape parity with the server's
/// config — the tests never read it.
#[allow(dead_code)]
mod excel_s3_test {
    #[derive(Clone)]
    pub struct S3Config {
        pub bucket: String,
        pub region: String,
        pub access_key_id: String,
        pub secret_access_key: String,
        pub endpoint_url: String,
        pub use_path_style: bool,
        pub allowed_key_prefixes: Vec<String>,
    }
}

fn free_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

#[test]
#[ignore = "needs Docker + integration feature"]
fn unauthenticated_call_is_rejected() {
    let stack = Stack::start().expect("stack");
    let client = reqwest::blocking::Client::new();
    let init = serde_json::json!({
        "jsonrpc": "2.0", "method": "initialize", "id": 1,
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "it", "version": "1"},
        },
    });
    let resp = client
        .post(&stack.mcp_base)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&init)
        .send()
        .unwrap();
    assert_eq!(resp.status(), 401, "no-token call must be rejected");
}

#[test]
#[ignore = "needs Docker + integration feature"]
fn initialize_reports_server_info() {
    let stack = Stack::start().expect("stack");
    let resp = stack
        .mcp_call(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "it", "version": "1"},
            }),
            1,
        )
        .unwrap();
    assert_eq!(
        resp["result"]["serverInfo"]["name"].as_str().unwrap(),
        "excel-mcp"
    );
}

#[test]
#[ignore = "needs Docker + integration feature"]
fn tools_list_exposes_exactly_three_tools() {
    let stack = Stack::start().expect("stack");
    let resp = stack
        .mcp_call("tools/list", serde_json::json!({}), 2)
        .unwrap();
    let mut names: Vec<String> = resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["create_xlsx", "sheet_apply", "sheet_read"]);
}

#[test]
#[ignore = "needs Docker + integration feature"]
fn create_read_apply_roundtrip() {
    let stack = Stack::start().expect("stack");

    // create: data lands in S3, small files come back inline
    let created = stack
        .tool_result_json(
            "create_xlsx",
            serde_json::json!({"data": [["name", "qty"], ["apple", 3], ["pear", 5]]}),
            10,
        )
        .unwrap();
    let s3_key = created["s3_key"].as_str().unwrap().to_string();
    assert!(s3_key.starts_with("excel-mcp-out/"), "key: {s3_key}");
    assert_eq!(created["cells_written"], 6);
    let b64 = created["content_b64"].as_str().unwrap();
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap();
    assert_eq!(&bytes[0..2], b"PK");

    // read: values from S3 through the engine
    let read = stack
        .tool_result_json("sheet_read", serde_json::json!({"s3_key": s3_key}), 11)
        .unwrap();
    assert_eq!(read["cells"]["A2"]["value"].as_str().unwrap(), "apple");
    assert_eq!(read["cells"]["B3"]["value"], 5);
    assert_eq!(read["values_are_cached"], false);

    // apply: edit into a new key, verify the object exists and holds the edit
    let applied = stack
        .tool_result_json(
            "sheet_apply",
            serde_json::json!({
                "s3_key": s3_key,
                "output_key": "excel-mcp-out/edited.xlsx",
                "cells": [{"address": "C2", "value": "fresh"}],
            }),
            12,
        )
        .unwrap();
    assert_eq!(
        applied["s3_key"].as_str().unwrap(),
        "excel-mcp-out/edited.xlsx"
    );
    assert_eq!(applied["cells_edited"], 1);

    let edited = stack.get_object("excel-mcp-out/edited.xlsx").unwrap();
    assert_eq!(&edited[0..2], b"PK");
    // Read the edited copy back through the tool to see the new cell.
    let read2 = stack
        .tool_result_json(
            "sheet_read",
            serde_json::json!({"s3_key": "excel-mcp-out/edited.xlsx"}),
            13,
        )
        .unwrap();
    assert_eq!(read2["cells"]["C2"]["value"].as_str().unwrap(), "fresh");

    // The source workbook was not modified by the apply.
    let read3 = stack
        .tool_result_json("sheet_read", serde_json::json!({"s3_key": s3_key}), 14)
        .unwrap();
    assert!(read3["cells"].get("C2").is_none());
}

#[test]
#[ignore = "needs Docker + integration feature"]
fn traversal_key_is_rejected() {
    let stack = Stack::start().expect("stack");
    let resp = stack
        .mcp_call(
            "tools/call",
            serde_json::json!({
                "name": "sheet_read",
                "arguments": {"s3_key": "../etc/passwd"},
            }),
            20,
        )
        .unwrap();
    // Key validation fails before the tool body runs, so rmcp surfaces it
    // as a JSON-RPC error object; a tool-level isError result is equally
    // acceptable. Anything else means the key slipped through.
    let rpc_error = resp.get("error").is_some();
    let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(rpc_error || is_error, "traversal key must fail: {resp}");
}

#[test]
#[ignore = "needs Docker + integration feature"]
fn seeded_external_object_reads_back() {
    let stack = Stack::start().expect("stack");

    // A workbook the agent did not create (built by the test, seeded direct).
    let sheets = excel_fixture::fixture_bytes();
    stack.seed_object("inputs/source.xlsx", sheets).unwrap();
    let read = stack
        .tool_result_json(
            "sheet_read",
            serde_json::json!({"s3_key": "inputs/source.xlsx"}),
            30,
        )
        .unwrap();
    assert_eq!(read["cells"]["A1"]["value"].as_str().unwrap(), "seeded");
}

#[test]
#[ignore = "needs Docker + integration feature"]
fn seeded_object_reads_back_via_url() {
    let stack = Stack::start().expect("stack");
    let sheets = excel_fixture::fixture_bytes();
    stack
        .seed_object("workspaces/ws-9/att.xlsx", sheets)
        .unwrap();
    // The URL form a platform agent would hold: endpoint + bucket + key.
    let url = format!("{}/uploads/workspaces/ws-9/att.xlsx", stack.s3_endpoint);
    let read = stack
        .tool_result_json("sheet_read", serde_json::json!({ "s3_key": url }), 40)
        .unwrap();
    assert_eq!(read["cells"]["A1"]["value"].as_str().unwrap(), "seeded");
}

/// Fixture builder local to the test crate (the binary crate's modules
/// are not exported; build a workbook with umya directly).
mod excel_fixture {
    pub fn fixture_bytes() -> Vec<u8> {
        // new_file(): blank workbook with one "Sheet1" already present.
        let mut book = umya_spreadsheet::new_file();
        let ws = book.sheet_mut(0).unwrap();
        ws.cell_mut("A1").set_value("seeded");
        ws.cell_mut("B1").set_value_number(42);
        let mut buf = std::io::Cursor::new(Vec::new());
        umya_spreadsheet::writer::xlsx::write_writer(&book, &mut buf).unwrap();
        buf.into_inner()
    }
}
