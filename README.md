# excel-mcp

Excel (.xlsx) workbook tools for AI agents, exposed as an MCP server over
Streamable HTTP. All file IO goes through S3 — agents pass `s3_key`s, never
local paths — which makes the server stateless and safe to run as a
multi-replica Kubernetes Deployment.

Three tools:

- **`create_xlsx`** — build a new workbook from a 2-D array (or several
  sheets). Strings starting with `=` become formulas. Returns the output
  `s3_key` plus the file bytes base64-inline when small.
- **`sheet_read`** — read one sheet: cells (values + formulas), merged
  ranges, sheet list. Values are recomputed with a formula engine
  ([IronCalc](https://ironcalc.com/)) when possible; cached values
  (flagged `values_are_cached: true`) otherwise. **`s3_key` also accepts
  the full object-store URL of an attachment** (e.g. a platform
  attachment's `url` field) — the server converts it to the key, so
  agents don't need to download or parse files locally.
- **`sheet_apply`** — edit an existing workbook (cell values/formulas,
  insert rows/columns, add/rename sheets) and write a new copy; the
  source object is never modified. Pass the source workbook's URL or
  S3 key as `s3_key` (URLs converted automatically).

## Example calls

`create_xlsx`:

```json
{
  "jsonrpc": "2.0", "id": 1, "method": "tools/call",
  "params": {
    "name": "create_xlsx",
    "arguments": {
      "data": [
        ["item", "qty", "total"],
        ["apple", 3, "=B2*2"]
      ],
      "sheet_name": "Sheet1",
      "output_key": "reports/fruit.xlsx"
    }
  }
}
```

Response: `{"s3_key": "reports/fruit.xlsx", "cells_written": 6,
"content_b64": "..."}` (`content_b64` only when the file is at or below the
inline size limit).

`sheet_read`:

```json
{
  "name": "sheet_read",
  "arguments": { "s3_key": "reports/fruit.xlsx", "sheet": "Sheet1", "max_rows": 500 }
}
```

`sheet_apply`:

```json
{
  "name": "sheet_apply",
  "arguments": {
    "s3_key": "reports/fruit.xlsx",
    "output_key": "reports/fruit-v2.xlsx",
    "cells": [{ "address": "B2", "value": 10 }],
    "ops": [{ "type": "insert_rows", "at": 4, "count": 1 }]
  }
}
```

## Configuration

Environment variables (see `.env.example` for the full annotated list):

| Variable | Default | Meaning |
|---|---|---|
| `MCP_HOST` | `0.0.0.0` | bind address |
| `MCP_PORT` | `8080` | listen port |
| `MCP_AUTH_KEY` | *(empty)* | Bearer token(s), comma-separated; empty disables auth |
| `S3_BUCKET` | — | **required**; bucket for all workbook IO |
| `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` | — | **required**; bucket credentials |
| `AWS_ENDPOINT_URL` | *(empty)* | MinIO/R2 endpoint; empty = AWS S3 |
| `S3_REGION` | `us-east-1` | region (falls back to `AWS_REGION`) |
| `S3_USE_PATH_STYLE` | auto | path-style iff `AWS_ENDPOINT_URL` is set |
| `S3_KEY_PREFIXES` | *(empty)* | segment-aware allowlist for readable input keys |
| `S3_OUTPUT_PREFIX` | `excel-mcp-out/` | prefix for generated output keys |
| `EXCEL_INLINE_MAX_BYTES` | `2097152` | base64 inline cutoff (2 MiB) |
| `EXCEL_MAX_FILE_BYTES` | `104857600` | upload/download cap (100 MiB) |

- `/health` is auth-exempt and returns `{"ok":true,"s3":"configured",
  "version":"0.1.3"}` — the `s3` field tells you whether the S3 env is
  complete.
- `/mcp` requires `Authorization: Bearer <key>` when `MCP_AUTH_KEY` is set.
- The MCP endpoint follows the Streamable HTTP transport: `initialize`,
  then subsequent calls carrying the `Mcp-Session-Id` response header.
  Responses may arrive as `text/event-stream` frames (`data: <json>`).

## Running locally

```bash
cargo run --release
# or with env:
MCP_AUTH_KEY=dev-key S3_BUCKET=uploads AWS_ACCESS_KEY_ID=... \
AWS_SECRET_ACCESS_KEY=... AWS_ENDPOINT_URL=http://localhost:9000 \
S3_USE_PATH_STYLE=true cargo run --release
```

Docker:

```bash
docker build -t excel-mcp -f deploy/Dockerfile .
docker run -p 8080:8080 -e MCP_AUTH_KEY=dev-key \
  -e S3_BUCKET=uploads -e AWS_ACCESS_KEY_ID=... -e AWS_SECRET_ACCESS_KEY=... \
  -e AWS_ENDPOINT_URL=http://host.docker.internal:9000 -e S3_USE_PATH_STYLE=true \
  excel-mcp
```

The runtime image is distroless (`cc-debian12:nonroot`): it ships CA
certificates, glibc and nothing else — no shell, so `kubectl exec -it … --
/bin/sh` won't work; use `kubectl logs` or a port-forward plus
`curl /health` instead. Local builds need network access to gcr.io for
the distroless base (CI builds it directly). The server runs as the
nonroot user (uid 65532).

Integration tests (`--ignored`) spin up MinIO via testcontainers and run the
full create→read→apply round trip:

```bash
cargo test --features integration -- --ignored
```

## 中文说明

### 项目定位

本服务把 Excel 的创建、读取、编辑能力封装成 MCP 工具，供其他容器里的
agent 远程调用。所有文件 IO 走 S3（推荐同集群 MinIO）：agent 只传
`s3_key`，不传本地路径，因此服务本身无状态，可以任意扩副本。

与相关项目的关系：

- **读 Office 文档转 Markdown** → `markitdown-mcp`
- **生成 Word 文档** → `genoffice-mcp`
- **读写 Excel** → 本项目（excel-mcp）

### 部署到 Kubernetes

```bash
# 1. 构建并推送镜像（或从 GitHub Release 下载 tarball 后 docker load）
docker build -t <registry>/excel-mcp:0.1.3 -f deploy/Dockerfile .
docker push <registry>/excel-mcp:0.1.3

# 2. 创建认证与 S3 凭证 secret
kubectl create secret generic excel-mcp-auth \
  --from-literal=auth-key='<your-key>'
kubectl create secret generic excel-mcp-s3 \
  --from-literal=AWS_ACCESS_KEY_ID='<access-key>' \
  --from-literal=AWS_SECRET_ACCESS_KEY='<secret-key>'

# 3. 按需修改 deploy/k8s.yaml 中的镜像地址和 MinIO endpoint
#    （endpoint 形如 http://minio.<namespace>.svc.cluster.local:9000，
#     并设置 S3_USE_PATH_STYLE=true），然后：
kubectl apply -f deploy/k8s.yaml
```

`deploy/k8s.yaml` 包含 2 副本 Deployment（`/health` 存活/就绪探针）与
Service。服务无状态，重启不丢数据（所有产物都在 S3）。

### Agent 侧接入

Agent 的 MCP 配置（Streamable HTTP，带 Bearer 认证）：

```json
{
  "mcpServers": {
    "excel": {
      "url": "http://excel-mcp.default.svc.cluster.local/mcp",
      "headers": {
        "Authorization": "Bearer <your-key>"
      }
    }
  }
}
```

接入后即可调用 `create_xlsx` / `sheet_read` / `sheet_apply` 三个工具。
`s3_key` 既接受 S3 key，也接受附件的完整对象存储 URL（如平台附件的
`url` 字段，形如 `http://minio:9000/uploads/workspaces/.../file.xlsx`），
服务端会自动去掉 endpoint 和 bucket 段转换成 key——agent 不需要下载
附件到本地解析。参数示例见上文英文部分的 Example calls。

## License

Apache-2.0. See [LICENSE](LICENSE).
