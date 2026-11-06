//! The MCP tool surface: one struct per tool's typed args, #[tool] methods
//! on a stateful `ExcelMcp` handler.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::tool;
use rmcp::{ErrorData as McpError, ServerHandler};
use rmcp::{tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::Config;

#[derive(Clone)]
pub struct ExcelMcp {
    pub cfg: Config,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateXlsxArgs {
    /// Rows as a 2-D array of cell values, or {"sheets":[{"name","rows"}]}.
    /// Strings starting with "=" are formulas.
    pub data: serde_json::Value,
    /// Optional sheet name when data is a bare 2-D array.
    #[serde(default)]
    pub sheet_name: Option<String>,
    /// Optional explicit output key (must end with .xlsx); default is
    /// generated under S3_OUTPUT_PREFIX.
    #[serde(default)]
    pub output_key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SheetReadArgs {
    /// S3 key of the workbook to read, or the full object-store URL of an
    /// attachment (converted to a key automatically).
    pub s3_key: String,
    /// Sheet name or 0-based index as string; default: first sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Row limit; extra rows are dropped and `truncated` is set.
    #[serde(default = "default_max_rows")]
    pub max_rows: usize,
    /// "json" (default) returns the full struct; "markdown" returns a table.
    #[serde(default)]
    pub format: Option<String>,
}

fn default_max_rows() -> usize {
    500
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SheetApplyArgs {
    /// S3 key of the source workbook, or its full object-store URL
    /// (converted to a key automatically). Never modified.
    pub s3_key: String,
    /// S3 key for the edited copy (required, must end with .xlsx).
    pub output_key: String,
    /// Default sheet for cell edits (name); default: first sheet.
    #[serde(default)]
    pub sheet: Option<String>,
    /// Cell edits: {address: "B3", value: ...}; "=" strings are formulas,
    /// null clears the cell.
    #[serde(default)]
    pub cells: Vec<crate::engine::apply::CellEdit>,
    /// Structural operations, applied after cell edits.
    #[serde(default)]
    pub ops: Vec<crate::engine::apply::WorkbookOp>,
}

#[tool_router]
impl ExcelMcp {
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    #[tool(
        description = "Create a new Excel (.xlsx) workbook from data: a 2-D array of cell values, or {\"sheets\":[{\"name\",\"rows\"}]} for several sheets. Strings starting with \"=\" are formulas. Prefer this over emitting CSV/markdown tables when the deliverable is a real spreadsheet. Returns the output s3_key and the file bytes base64-encoded when small."
    )]
    async fn create_xlsx(
        &self,
        Parameters(args): Parameters<CreateXlsxArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut sheets = crate::engine::create::parse_data(&args.data)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        // A bare 2-D array plus sheet_name: name the only sheet.
        if let (Some(name), true) = (args.sheet_name.clone(), sheets.len() == 1)
            && sheets[0].name.is_none()
        {
            sheets[0].name = Some(name);
        }
        let cells = crate::engine::create::count_cells(&sheets);
        let bytes = tokio::task::spawn_blocking(move || crate::engine::create::build_xlsx(&sheets))
            .await
            .map_err(|_| McpError::internal_error("engine task failed", None))?
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Resolve output key: explicit (validated) or generated.
        let output_key = match args.output_key.as_deref() {
            Some(explicit) => crate::s3keys::normalize_output_key(explicit)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?,
            None => crate::storage::generated_output_key(&self.cfg.output_prefix),
        };
        let s3 = self.cfg.s3.clone().ok_or_else(|| {
            McpError::internal_error(
                "S3 output is not configured on this server; ask the operator to set S3_BUCKET and credentials",
                None,
            )
        })?;
        let store = crate::storage::client(&s3)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let store: Box<dyn object_store::ObjectStore> = Box::new(store);
        let max = self.cfg.max_file_bytes;
        if bytes.len() as u64 > max {
            return Err(McpError::internal_error(
                format!("output exceeds the {max} byte limit"),
                None,
            ));
        }
        crate::storage::upload_bytes(store.as_ref(), &output_key, bytes.clone())
            .await
            .map_err(|_| McpError::internal_error("upload failed", None))?;

        let inline_limit = self.cfg.inline_max_bytes as usize;
        let mut obj = serde_json::json!({ "s3_key": output_key, "cells_written": cells });
        if bytes.len() <= inline_limit {
            obj["content_b64"] = serde_json::Value::String(crate::util::base64_encode(&bytes));
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            obj.to_string(),
        )]))
    }

    #[tool(
        description = "Read one sheet of an .xlsx workbook from S3. Use this whenever you need data from an Excel file, including platform attachments whose URL points at the object store: pass the attachment's URL or its S3 key as s3_key (URLs are converted to keys automatically). Do NOT download attachments or parse xlsx files locally. Returns sheet list, cells (A1-keyed values plus formulas), merged ranges and a truncation flag. Values are recomputed with a formula engine when possible, else cached values are returned (values_are_cached=true)."
    )]
    async fn sheet_read(
        &self,
        Parameters(args): Parameters<SheetReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let s3 = self.cfg.s3.clone().ok_or_else(|| {
            McpError::internal_error(
                "S3 output is not configured on this server; ask the operator to set S3_BUCKET and credentials",
                None,
            )
        })?;
        let resolved_key = crate::s3keys::resolve_input(&args.s3_key, &s3)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let store = crate::storage::client(&s3)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let store: Box<dyn object_store::ObjectStore> = Box::new(store);
        let max = self.cfg.max_file_bytes;
        let tmp = crate::storage::fetch_to_tempfile(store.as_ref(), &resolved_key, max)
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let sheet = args.sheet.clone();
        let max_rows = args.max_rows;
        let result = tokio::task::spawn_blocking(move || {
            crate::engine::read::read_sheet(tmp.as_path(), sheet.as_deref(), max_rows)
        })
        .await
        .map_err(|_| McpError::internal_error("engine task failed", None))?
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let text = match args.format.as_deref() {
            Some("markdown") => crate::engine::read::to_markdown(&result),
            _ => serde_json::to_string(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?,
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Edit an .xlsx workbook from S3 and write a new copy: set cell values/formulas, insert rows/columns, add or rename sheets. The source object is never modified. Pass the source workbook's URL or S3 key as s3_key (URLs are converted to keys automatically); output_key must be an S3 key ending in .xlsx."
    )]
    async fn sheet_apply(
        &self,
        Parameters(args): Parameters<SheetApplyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let s3 = self.cfg.s3.clone().ok_or_else(|| {
            McpError::internal_error(
                "S3 output is not configured on this server; ask the operator to set S3_BUCKET and credentials",
                None,
            )
        })?;
        let resolved_key = crate::s3keys::resolve_input(&args.s3_key, &s3)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let output_key = crate::s3keys::normalize_output_key(&args.output_key)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let store = crate::storage::client(&s3)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let store: Box<dyn object_store::ObjectStore> = Box::new(store);
        let max = self.cfg.max_file_bytes;
        let tmp = crate::storage::fetch_to_tempfile(store.as_ref(), &resolved_key, max)
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let sheet = args.sheet.clone();
        let cells = args.cells;
        let ops = args.ops;
        let ops_count = ops.len();
        let (edited_bytes, applied) = tokio::task::spawn_blocking(move || {
            let dst = tempfile::NamedTempFile::new()
                .map_err(|e| crate::engine::apply::ApplyError::Open(e.to_string()))?;
            let applied = crate::engine::apply::apply_edits(
                tmp.as_path(),
                dst.path(),
                sheet.as_deref(),
                &cells,
                &ops,
            )?;
            std::fs::read(dst.path())
                .map_err(|e| crate::engine::apply::ApplyError::Open(e.to_string()))
                .map(|b| (b, applied))
        })
        .await
        .map_err(|_| McpError::internal_error("engine task failed", None))?
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        if edited_bytes.len() as u64 > max {
            return Err(McpError::internal_error(
                format!("output exceeds the {max} byte limit"),
                None,
            ));
        }
        crate::storage::upload_bytes(store.as_ref(), &output_key, edited_bytes)
            .await
            .map_err(|_| McpError::internal_error("upload failed", None))?;

        let obj = serde_json::json!({
            "s3_key": output_key,
            "cells_edited": applied,
            "ops_applied": ops_count,
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            obj.to_string(),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for ExcelMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Excel workbook tools over S3 — use them instead of parsing xlsx files yourself. \
                 sheet_read returns cells, formulas and recomputed values for one sheet; \
                 sheet_apply edits an existing workbook and writes a new copy; \
                 create_xlsx builds a new .xlsx from data. All inputs are S3 keys, but full \
                 object-store URLs (e.g. platform attachment URLs) are accepted and converted \
                 automatically — do not download attachments to parse them locally.",
            )
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> Config {
        Config {
            host: "127.0.0.1".into(),
            port: 0,
            auth_keys: vec![],
            s3: None,
            output_prefix: "excel-mcp-out/".into(),
            inline_max_bytes: 1024 * 1024,
            max_file_bytes: 1024 * 1024,
        }
    }

    fn extract_text(result: &CallToolResult) -> Option<String> {
        match result.content.first() {
            Some(ContentBlock::Text(t)) => Some(t.text.clone()),
            _ => None,
        }
    }

    #[tokio::test]
    async fn create_xlsx_errors_without_s3() {
        let server = ExcelMcp::new(test_cfg());
        let result = server
            .create_xlsx(Parameters(CreateXlsxArgs {
                data: serde_json::json!([[1, 2], [3, "=SUM(A1:B1)"]]),
                sheet_name: None,
                output_key: None,
            }))
            .await;
        // The not-configured error surfaces as an MCP error.
        let msg = match result {
            Err(e) => e.message.to_string(),
            Ok(r) => {
                if r.is_error.unwrap_or(false) {
                    extract_text(&r).unwrap_or_default()
                } else {
                    String::new()
                }
            }
        };
        assert!(msg.contains("not configured"), "got: {msg}");
    }

    #[tokio::test]
    async fn create_xlsx_rejects_bad_output_key() {
        // With S3 configured (but unreachable), a bad output key must be
        // rejected before any network call.
        let mut cfg = test_cfg();
        cfg.s3 = Some(crate::config::S3Config {
            bucket: "b".into(),
            region: "us-east-1".into(),
            access_key_id: "ak".into(),
            secret_access_key: "sk".into(),
            endpoint_url: "http://127.0.0.1:1".into(),
            use_path_style: true,
            allowed_key_prefixes: vec![],
        });
        let server = ExcelMcp::new(cfg);
        let result = server
            .create_xlsx(Parameters(CreateXlsxArgs {
                data: serde_json::json!([[1]]),
                sheet_name: None,
                output_key: Some("not-an-xlsx.csv".into()),
            }))
            .await;
        let msg = match result {
            Err(e) => e.message.to_string(),
            Ok(r) => {
                if r.is_error.unwrap_or(false) {
                    extract_text(&r).unwrap_or_default()
                } else {
                    String::new()
                }
            }
        };
        assert!(
            msg.contains(".xlsx"),
            "expected output-key rejection, got: {msg}"
        );
    }

    #[tokio::test]
    async fn sheet_read_rejects_dangerous_key() {
        // Path-traversal keys must be rejected before any S3 call.
        let mut cfg = test_cfg();
        cfg.s3 = Some(crate::config::S3Config {
            bucket: "b".into(),
            region: "us-east-1".into(),
            access_key_id: "ak".into(),
            secret_access_key: "sk".into(),
            endpoint_url: "http://127.0.0.1:1".into(),
            use_path_style: true,
            allowed_key_prefixes: vec![],
        });
        let server = ExcelMcp::new(cfg);
        let result = server
            .sheet_read(Parameters(SheetReadArgs {
                s3_key: "../etc/passwd".into(),
                sheet: None,
                max_rows: 500,
                format: None,
            }))
            .await;
        let msg = match result {
            Err(e) => e.message.to_string(),
            Ok(r) => {
                if r.is_error.unwrap_or(false) {
                    extract_text(&r).unwrap_or_default()
                } else {
                    String::new()
                }
            }
        };
        assert!(
            msg.contains("..") || msg.contains("key"),
            "expected input-key rejection, got: {msg}"
        );
    }

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
            !msg.contains("does not match bucket")
                && !msg.contains("no object key")
                && !msg.contains("invalid s3 key"),
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

    #[tokio::test]
    async fn sheet_apply_accepts_url_and_passes_validation() {
        // Mirror of the sheet_read accept test: the URL converts to a key,
        // validation passes, and the failure (if any) comes from the
        // unreachable endpoint — not from key rejection.
        let server = ExcelMcp::new(url_cfg());
        let result = server
            .sheet_apply(Parameters(SheetApplyArgs {
                s3_key: "http://127.0.0.1:1/uploads/workspaces/a.xlsx".into(),
                output_key: "out/a.xlsx".into(),
                sheet: None,
                cells: vec![],
                ops: vec![],
            }))
            .await;
        let msg = match &result {
            Err(e) => e.message.to_string(),
            Ok(r) => extract_text(r).unwrap_or_default(),
        };
        assert!(
            !msg.contains("does not match bucket")
                && !msg.contains("no object key")
                && !msg.contains("invalid s3 key"),
            "URL should have been accepted, got: {msg}"
        );
    }
}
