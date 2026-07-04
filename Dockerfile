# Dockerfile for reproducible builds of brainstem-daemon
#
# Supports both configurations:
#   - Core only (no libzmq):  docker build --target core .
#   - Full (with corpus-ipc): docker build --target full  .
#
# CI and contributors can validate:
#   cargo fmt --check, clippy, build, test inside the image.

FROM rust:1.80-bookworm AS base
WORKDIR /app
# Common system deps for the full feature set (libzmq). Core-only builds do not need this.
RUN apt-get update && apt-get install -y --no-install-recommends \
    libzmq3-dev=4.3.4-6 \
    pkg-config=1.8.1-1 \
    ca-certificates=20230311+deb12u1 \
 && rm -rf /var/lib/apt/lists/*

# Copy manifests first for better layer caching
COPY Cargo.toml Cargo.lock ./
# Create a dummy main to cache dependencies
RUN mkdir -p src/bin && \
    echo 'fn main(){}' > src/bin/soma_daemon.rs && \
    echo 'pub fn _dummy(){}' > src/lib.rs && \
    cargo fetch

# ---- Core build (no external ZMQ) ----
FROM base AS core
# Remove the dummy to force re-copy of real sources
RUN rm -rf src
COPY . .
# Verify core-only works without libzmq at runtime (build-time still had it for fetch, but we can also test a pure check)
RUN cargo check --no-default-features && \
    cargo clippy --all-targets --no-default-features -- -D warnings && \
    cargo test --no-default-features

# ---- Full build (with corpus-ipc + zmq) ----
FROM base AS full
RUN rm -rf src
COPY . .
RUN cargo check --all-features && \
    cargo clippy --all-targets --all-features -- -D warnings && \
    cargo test --all-features

# Default target builds the full image
FROM full AS final
# Run as non-root for security scanners (CodeRabbit/CodeAnt/Cursor).
# Previous stages compile as root. We produce a release binary here and
# switch to non-root. The CMD runs the pre-built binary directly (no cargo
# at runtime), so there is no write to target/ from the final image.
RUN useradd -m -u 10001 appuser 2>/dev/null || true
RUN cargo build --release --features corpus-ipc && \
    chown -R appuser:appuser /app/target /usr/local/cargo 2>/dev/null || true
USER appuser
WORKDIR /app
CMD ["/app/target/release/soma-daemon", "--help"]
