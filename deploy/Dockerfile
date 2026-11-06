# excel-mcp container image.
# Built in CI for linux/amd64 and linux/arm64; also buildable locally
# (requires network access to gcr.io for the distroless base):
#   docker build -t excel-mcp -f deploy/Dockerfile .
FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Build dependencies only; this layer caches independently of source changes.
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin excel-mcp

# Distroless runtime: CA certs, libc/libm/libgcc_s, tzdata — no shell,
# no package manager, runs as nonroot (uid 65532). Debug via
# `kubectl logs` or port-forward + curl /health, not kubectl exec.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=builder /app/target/release/excel-mcp /usr/local/bin/excel-mcp

ENV MCP_HOST=0.0.0.0 \
    MCP_PORT=8080

EXPOSE 8080

# No Docker HEALTHCHECK: Kubernetes liveness/readiness probes on /health
# (see deploy/k8s.yaml) are the real contract; plain `docker run` users
# can curl /health themselves.

CMD ["excel-mcp"]
