# Distroless Image + Slim Binary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut the container image from ~136MB to ~40MB by switching the runtime base to distroless and stripping/LTO'ing the binary, with zero behavior change.

**Architecture:** Two independent levers, landed as two commits: (1) Cargo profile + feature change that removes the C zlib link and shrinks the binary 41%; (2) Dockerfile runtime-stage swap to `gcr.io/distroless/cc-debian12:nonroot`. CI's existing smoke job is the end-to-end gate.

**Tech Stack:** Rust 2024 / cargo feature unification, Docker multi-stage build, distroless cc-debian12 (nonroot, uid 65532).

**Spec:** `docs/superpowers/specs/2026-11-06-distroless-image-design.md`

---

## Task 1: Drop zlib feature + release profile

**Files:**
- Modify: `Cargo.toml` (line 13: remove `features = ["zlib"]`; append `[profile.release]`)

All verification steps below were already run manually during design; the implementer re-runs them to confirm the committed state.

- [ ] **Step 1: Edit Cargo.toml**

Line 13, change:

```toml
rust_xlsxwriter = { version = "0.102", features = ["zlib"] }
```

to:

```toml
rust_xlsxwriter = { version = "0.102" }
```

Append at end of file:

```toml

[profile.release]
strip = true
lto = true
codegen-units = 1
```

- [ ] **Step 2: Refresh lockfile, verify libz-sys is gone**

```bash
cargo update -p rust_xlsxwriter 2>&1 | tail -1   # triggers re-resolution
cargo tree -i libz-sys -e no-dev
```

Expected: the second command FAILS with `error: package ID specification 'libz-sys' did not match any packages` — that error IS the pass condition (libz-sys left the graph). If instead it prints a tree, the feature didn't drop: report BLOCKED.

Also confirm the deflate backend unified on zlib-rs:

```bash
cargo tree -e features -i zlib-rs 2>&1 | grep "deflate-flate2-zlib-rs" | head -2
```

Expected: zip 8.6.0 and/or zip 9.3.0 edges through `deflate-flate2-zlib-rs`.

- [ ] **Step 3: Build + full test suite**

```bash
cargo build --release 2>&1 | tail -1
cargo test 2>&1 | grep -E "^test result" | head -3
```

Expected: build succeeds; **58 passed, 0 failed** (same as main). Note: `cargo test` uses the dev profile — unaffected by the new release profile; the count must not move.

- [ ] **Step 4: Verify binary size + no libz link**

```bash
ls -la target/release/excel-mcp
otool -L target/release/excel-mcp | grep -ci libz || true
```

Expected (mac arm64): size ≈ 16–17MB (was 28.6MB); libz grep count **0**. On this machine otool exists; if not, `objdump -p target/release/excel-mcp | grep NEEDED` and expect no libz NEEDED entry.

- [ ] **Step 5: fmt + clippy**

```bash
cargo fmt --check
cargo clippy --all-targets 2>&1 | grep -c "^warning\|^error"
```

Expected: fmt clean; count 0.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: pure-Rust zlib backend, stripped LTO release binary"
```

End the commit message with "Co-Authored-By: Claude Code <noreply@anthropic.com>".

---

## Task 2: Distroless runtime stage

**Files:**
- Modify: `deploy/Dockerfile` (runtime stage only)

- [ ] **Step 1: Rewrite the runtime stage**

Replace:

```dockerfile
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
  && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/excel-mcp /usr/local/bin/excel-mcp
```

with:

```dockerfile
# Distroless runtime: CA certs, libc/libm/libgcc_s, tzdata — no shell,
# no package manager, runs as nonroot (uid 65532). Debug via
# `kubectl logs` or port-forward + curl /health, not kubectl exec.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=builder /app/target/release/excel-mcp /usr/local/bin/excel-mcp
```

Keep everything else in the file byte-identical: the header comment (including "also buildable locally"), chef/planner/builder stages, ENV, EXPOSE, the "No Docker HEALTHCHECK" comment, CMD. Update the header comment's second line to note local builds need gcr.io reachability:

```dockerfile
# excel-mcp container image.
# Built in CI for linux/amd64 and linux/arm64; also buildable locally
# (requires network access to gcr.io for the distroless base):
#   docker build -t excel-mcp -f deploy/Dockerfile .
```

- [ ] **Step 2: Dockerfile lint (hadolint if available, else manual)**

```bash
which hadolint && hadolint deploy/Dockerfile || echo "hadolint not installed — manual review only"
```

Manual review checklist regardless: `nonroot` tag present; no `apt-get` left; `USER` NOT set (image provides uid 65532); exactly one `COPY --from=builder`.

- [ ] **Step 3: Local docker build — expected to fail on network, skip with note**

```bash
timeout 60 docker build -t excel-mcp-distroless-check -f deploy/Dockerfile . 2>&1 | tail -3
```

Expected on this machine: fails pulling `gcr.io/distroless/cc-debian12:nonroot` (network unreachable; local mirrors don't carry distroless). This is a KNOWN, ACCEPTED skip — CI (GitHub Actions runners) pulls gcr.io directly and is the authoritative gate (its smoke job boots the image against moto and exercises the full MCP surface). Record the failure output in the report; do NOT treat it as a task blocker, and do NOT change the Dockerfile to work around it.

If the pull unexpectedly succeeds (proxy present), let the build run and verify the container starts:

```bash
docker run -d --name distroless-check -e S3_BUCKET=x \
  -e AWS_ACCESS_KEY_ID=x -e AWS_SECRET_ACCESS_KEY=x -p 18100:8080 excel-mcp-distroless-check
sleep 2 && curl -s http://127.0.0.1:18100/health
docker rm -f distroless-check
```

Expected health JSON: `{"ok":true,"s3":"configured",...}`.

- [ ] **Step 4: Commit**

```bash
git add deploy/Dockerfile
git commit -m "build: distroless nonroot runtime, drop apt layer"
```

End the commit message with "Co-Authored-By: Claude Code <noreply@anthropic.com>".

---

## Task 3: README + version bump

**Files:**
- Modify: `README.md` (Docker section; version strings)
- Modify: `Cargo.toml:3` (0.1.2 → 0.1.3), `Cargo.lock`

- [ ] **Step 1: Update the English Docker section**

Current block (lines ~110-124):

```markdown
Docker:

```bash
docker build -t excel-mcp -f deploy/Dockerfile .
docker run -p 8080:8080 -e MCP_AUTH_KEY=dev-key \
  -e S3_BUCKET=uploads -e AWS_ACCESS_KEY_ID=... -e AWS_SECRET_ACCESS_KEY=... \
  -e AWS_ENDPOINT_URL=http://host.docker.internal:9000 -e S3_USE_PATH_STYLE=true \
  excel-mcp
```
```

Insert a short paragraph between the code block and "Integration tests":

```markdown
The runtime image is distroless (`cc-debian12:nonroot`): it ships CA
certificates, glibc and nothing else — no shell, so `kubectl exec -it … --
/bin/sh` won't work; use `kubectl logs` or a port-forward plus
`curl /health` instead. Local builds need network access to gcr.io for
the distroless base (CI builds it directly). The server runs as the
nonroot user (uid 65532).
```

- [ ] **Step 2: Version bump**

`Cargo.toml` line 3: `version = "0.1.2"` → `version = "0.1.3"`. Then `cargo check` to refresh `Cargo.lock` (expect excel-mcp 0.1.3).

- [ ] **Step 3: Sweep version strings in README**

```bash
grep -n "0\.1\.2" README.md
```

Update every hit to 0.1.3: the `/health` sample response (~line 93) and both image-tag lines (~145-146). After edits, `grep -n "0\.1\.2" README.md` must return nothing.

- [ ] **Step 4: Full verification**

```bash
cargo test 2>&1 | grep -E "^test result" | head -3
cargo fmt --check
grep -n "0\.1\.2" README.md || echo "no stale versions"
grep -A1 'name = "excel-mcp"' Cargo.lock | grep version
```

Expected: 58 passed / 0 failed; fmt clean; no stale 0.1.2; lock says 0.1.3.

- [ ] **Step 5: Commit**

```bash
git add README.md Cargo.toml Cargo.lock
git commit -m "docs+chore: distroless runtime notes, bump 0.1.3"
```

End the commit message with "Co-Authored-By: Claude Code <noreply@anthropic.com>".

---

## Self-Review Checklist (done during plan writing)

- Spec coverage: base image swap (Task 2), profile+feature (Task 1), docs/notes incl. no-shell caveat and gcr.io reachability (Task 3), version bump (Task 3), CI untouched (explicitly no task touches workflows). k8s.yaml untouched per user decision. aws-lc-rs untouched per spec.
- Placeholders: none — every step has verbatim text or exact commands with expected outputs.
- Type consistency: N/A (no signatures). File paths verified against current tree.
- Local-network skip for docker build is pre-analyzed with an acceptance path, not a TBD.
