# Distroless Image + Slim Binary

Ship the excel-mcp container as a distroless image (~40MB instead of
~136MB) with a stripped, LTO'd binary — same behavior, same security
rules, same CI contract.

## Background

The current image is `debian:bookworm-slim` + an `apt-get` layer for
`ca-certificates` (136MB total, ~97MB of which is the base). The
release binary is 28.6MB unstripped. Two changes shrink both layers:

1. **Compiler profile.** `strip = true`, `lto = true`,
   `codegen-units = 1` in `[profile.release]` — measured locally:
   28.6MB → 16.8MB (mac arm64; linux/amd64 similar).
2. **Base image.** `gcr.io/distroless/cc-debian12:nonroot` replaces
   debian-slim + apt. Distroless ships CA certs, libc/libm/libgcc_s,
   and nothing else; no shell, no package manager.

## Prerequisite verified locally

Dropping `rust_xlsxwriter`'s `zlib` feature removes the C `libz-sys`
from the dependency graph entirely (verified with `cargo tree -i
libz-sys` + `otool -L` on the built binary). The zip/deflate backend
then unifies on `zlib-rs` (pure Rust, near-C performance) — the same
backend `calamine`, `ironcalc`, and `umya-spreadsheet` already use, so
all three engines share one compression path. The full test suite
passes without the feature (58/58, debug and release).

Note: `aws-lc-sys` (pulled by `object_store/aws` for TLS crypto)
remains a C library, but it links statically into the binary — no
runtime .so dependency — so distroless/cc covers it. It stays as-is:
AWS's default provider, best S3 compatibility.

## Goals

- Image ≤ ~45MB (from ~136MB); release tarball ~15MB (from ~40MB).
- No behavior change: tools, auth, S3 IO, tests, smoke — all identical.
- CI (GitHub Actions) keeps working unmodified: runners pull gcr.io
  directly; smoke tests are host-side HTTP, unaffected by no-shell.

## Non-goals

- Changing k8s.yaml (distroless nonroot runs as uid 65532 by default;
  no securityContext is added).
- Replacing aws-lc-rs with ring (churn without measurable size win).
- Local-airgap buildability: local `docker build` needs network access
  to gcr.io (or a proxy); CI is the canonical build path. README notes
  this.

## Design

### 1. Cargo.toml

```toml
rust_xlsxwriter = { version = "0.102" }   # was: features = ["zlib"]

[profile.release]
strip = true
lto = true
codegen-units = 1
```

The `zlib` feature mapped to `zip/deflate-flate2-zlib` → `flate2/zlib`
→ `libz-sys` (C). Without it, feature unification resolves
`deflate-flate2-zlib-rs` for every consumer, and `libz-sys` drops out
of the lockfile.

### 2. deploy/Dockerfile

The runtime stage becomes:

```dockerfile
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=builder /app/target/release/excel-mcp /usr/local/bin/excel-mcp
```

- The `apt-get update && apt-get install ca-certificates` layer is
  deleted: cc-debian12 already contains /etc/ssl/certs.
- `USER` is not set: the nonroot tag already runs as uid 65532
  (nonroot:nonroot).
- Builder stages (cargo-chef) unchanged — they still need a full
  toolchain; only the shipped layer changes.
- `EXPOSE 8080`, `ENV`, `CMD` unchanged. The "No Docker HEALTHCHECK"
  comment stays true.

### 3. Docs

README: update the Docker section — mention the distroless base (no
shell: `kubectl exec -it … -- /bin/sh` won't work; use `kubectl logs` /
port-forward + curl /health), and that local builds need gcr.io
reachability (CI is unaffected). No Chinese-section change beyond the
image tags if version numbers appear.

## Testing

- `cargo test` — 58/58 (already verified on the zlib-rs backend in
  both profiles).
- `cargo fmt --check`, `cargo clippy --all-targets -D warnings` — clean
  (CI gates).
- `cargo tree -i libz-sys` — errors with "did not match any packages"
  (the dependency is gone). Codified as a one-line check in the plan,
  not a unit test (dependency-graph property, not runtime behavior).
- Integration suite (`--features integration -- --ignored`) — runs the
  full MinIO round trip on the new compression backend. (Docker
  available locally; MinIO image already cached.)
- CI smoke job — unchanged and authoritative: builds the distroless
  image, boots it against moto, exercises initialize → tools/list →
  create → read → apply over HTTP. If the image were missing CA certs
  or libc, TLS/S3 calls would fail here.
- Local docker build — blocked by network (gcr.io unreachable from
  this machine, mirrors don't carry distroless); noted as a skip with
  rationale, CI validates the Dockerfile instead.

## Rollout

Normal version bump (0.1.2 → 0.1.3) and tag. The image tag reference
in README examples updates. K8s deployments pull the new tag; nothing
else changes for operators. Rollback = redeploy previous tag.
