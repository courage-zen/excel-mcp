# Agent-First Tool Descriptions + URL-Tolerant s3_key

Make the MCP surface self-explanatory enough that an agent holding nothing but
an attachment URL reaches for `sheet_read` instead of downloading the file and
parsing it locally.

## Background

A platform agent was asked to analyze an xlsx attachment. Instead of calling
`sheet_read`, it downloaded the attachment and tried to parse it with local
scripts — which failed (no python/unzip, no network for npm). The attachment's
`url` field points at the platform MinIO; the correct move was one
`sheet_read` call with the S3 key. Two gaps in the current tool surface caused
the detour:

1. **No "when to use" guidance.** Tool descriptions state what each tool does,
   never when an agent should prefer it over local parsing. The server
   `instructions` field is three dry sentences.
2. **No URL → key mapping.** Agents hold a full URL
   (`http://minio:9000/bucket/workspaces/…/file.xlsx`). Nothing told them to
   strip the endpoint/bucket prefix, so even a willing agent couldn't produce
   a valid `s3_key` without guessing.

## Goals

- An agent that sees an xlsx attachment URL calls `sheet_read` directly.
- `s3_key` accepts a full URL and converts it to a key server-side; agents
  need zero URL-parsing reasoning.
- Security boundary unchanged: extracted keys pass the full
  `validate_input_key` rule set (traversal, allowlist, dangerous extensions).

## Non-goals

- MCP resources (attachment discovery) — depends on the platform agent
  framework; not needed to fix this failure mode.
- Fetching xlsx over arbitrary external HTTP URLs (S3 object store only; the
  URL is a key encoding, not a fetch target).
- Multi-language descriptions (English only; best instruction-following).

## Design

### 1. Rewritten tool copy (English)

Each `#[tool(description = …)]` gains a WHEN clause and agent-relevant rules.
Draft copy:

**`sheet_read`** — *Read one sheet of an .xlsx workbook from S3. Use this
whenever you need data from an Excel file — including platform attachments
whose URL points at the object store. Pass the attachment's URL or its S3 key
as `s3_key`; URLs are converted to keys automatically. Do NOT download
attachments or parse xlsx locally. Returns sheet list, cells (A1-keyed values
plus formulas), merged ranges and a truncation flag. Values are recomputed
with a formula engine when possible, else cached values are returned
(`values_are_cached=true`).*

**`sheet_apply`** — *Edit an .xlsx workbook from S3 and write a new copy: set
cell values/formulas, insert rows/columns, add or rename sheets. The source
object is never modified; requires an explicit output_key ending in .xlsx.
Pass the source as URL or S3 key (URLs converted automatically).*

**`create_xlsx`** — *Create a new Excel (.xlsx) workbook from data: a 2-D
array of cell values, or `{"sheets":[{"name","rows"}]}` for several sheets.
Strings starting with "=" are formulas. Prefer this over building CSV/markdown
tables when the deliverable is a real spreadsheet. Returns the output s3_key
and the file bytes base64-encoded when small.*

**Server `instructions`** (initialize response) — one compact paragraph:
tools' division of labor, "all Excel IO goes through this server — never
download xlsx attachments to parse locally", and the URL-tolerance note.
Roughly 60 words; instructions are cheap, tool schemas are re-sent often.

### 2. URL-tolerant s3_key (`src/s3keys.rs`)

New function `resolve_input(raw: &str, s3: &S3Config) -> Result<String, InvalidKey>`:

- If `raw` doesn't start with `http://` or `https://`, behave exactly as
  `validate_input_key` today (plain key path unchanged).
- Otherwise parse the URL with a small hand-rolled splitter (scheme, host,
  path; query string dropped) — no new crates:
  1. Drop `?<query>` first, then split off `scheme://`, take the remainder,
     split at the first `/` → host, rest-of-path.
  2. **Virtual-host style**: if the host starts with `<bucket>.` (the
     configured bucket), the path minus leading `/` is the key.
  3. **Path style**: the path minus leading `/` is `bucket/key`; strip the
     first segment if it equals the configured bucket. If the first segment
     is *not* the bucket, the URL is not ours → error (see below).
  4. Percent-decoding is not applied — S3 keys with spaces/unicode arrive
     percent-encoded and MinIO accepts the encoded form; decoding risks
     reintroducing `/` or `..` from `%2F`/`%2E`. (Keys stay byte-exact.)
  5. If no path remains after the host (bare `http://host:9000`), error:
     "URL contains no object key".
- The extracted key then passes through `validate_input_key` unchanged:
  traversal segments, dangerous extensions, allowlist prefixes. A traversal
  smuggled via URL (`http://h/bucket/../etc/passwd`) is caught here.
- Query strings (presign signatures `?X-Amz-…`) are dropped before parsing.
- Errors are agent-actionable: when extraction fails, the message names the
  problem and the expected shape ("URL first path segment is not bucket
  'uploads'; expected http…/uploads/<key>").

`tools.rs` call sites change from
`validate_input_key(&args.s3_key, &s3)` to `resolve_input(&args.s3_key, &s3)`
in `sheet_read` and `sheet_apply`. `create_xlsx` output keys are unaffected
(agents never hold URLs for outputs).

Result payloads gain no changes: tools already return `s3_key`, so an agent
that passed a URL gets back the canonical key for follow-up calls.

### 3. Arg doc comments

The `s3_key` doc comments on `SheetReadArgs` / `SheetApplyArgs` update to:
"S3 key of the workbook, or the full object-store URL of an attachment
(converted automatically)". These become JSON-schema field descriptions the
agent sees at parameter level — the last nudge at the moment of filling in
the value.

## Testing

- **Unit** (`s3keys.rs`): path-style URL, virtual-host URL, URL with query
  string, plain key passthrough, URL with traversal in key, URL whose first
  segment ≠ bucket, non-URL garbage (falls through to key validation).
- **Unit** (`tools.rs`): `sheet_read` with a URL-shaped `s3_key` passes key
  validation (fails later at unreachable S3, proving validation passed); a
  URL with a traversal payload is rejected with a key-rule message.
- **Integration** (existing `--ignored` suite): one new case — seed an object,
  read it back passing the full MinIO URL (built from the stack's
  `s3_endpoint`) as `s3_key`; assert the read succeeds.
- **Live check**: after implementation, call the running local server once
  (curl or the session's MCP client) to confirm the new descriptions appear
  in `tools/list` and a URL passes.

## Rollout

No config or protocol changes; deploy as a normal version bump. Existing
agents passing plain keys are unaffected (passthrough path). The first agent
that passes a URL gets the converted key echoed in the result.
