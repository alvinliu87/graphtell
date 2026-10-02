# GraphTell -- single-container deployment (backend + built web UI).
#
# Architecture: the backend `graphtell serve` serves both the REST API and the React SPA on 5177 (--ui-dir),
# single port, same origin, no CORS, no extra reverse proxy needed.
#
# Build (model-candle / model-ort on by default; recall uses real bge-m3 semantic vectors, falling back to lexical when weights are missing):
#   docker build -t graphtell:latest .
#
# To build a smaller "lexical-only" image (no candle / ort compile), turn off the default features with a build arg:
#   docker build -t graphtell:hash --build-arg GT_FEATURES="--no-default-features" .
#
# Run:
#   docker run -d -p 5177:5177 -v $(pwd)/data:/data graphtell:latest
#   open http://localhost:5177/ in a browser

############################ Stage 1: build frontend ############################
FROM node:20-slim AS ui-builder
WORKDIR /src/ui
# The same lockfile drives a reproducible install
COPY ui/package.json ui/package-lock.json ./
RUN npm ci
COPY ui/ ./
# Same-origin deployment: the API uses relative paths, no hostname hardcoded at build time
ENV VITE_API_BASE=same-origin
RUN npm run build

############################ Stage 2: build backend ############################
FROM rust:1.83-slim AS rust-builder
# Local build deps for model-candle / model-ort
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config cmake \
        libssl-dev clang libclang-dev protobuf-compiler \
        libsqlite3-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# Use a build arg to control features (empty by default = use the default features in Cargo.toml: candle + ort)
ARG GT_FEATURES=""
RUN cargo build --release ${GT_FEATURES} -p gt-app

############################ Stage 3: runtime image ############################
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        libssl3 libgcc-s1 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

# Backend binary
COPY --from=rust-builder /src/target/release/graphtell /app/graphtell
# Built-in assets (FKB / rules / perspective declarations): the binary resolves them relative to the executable's directory
COPY fkb /app/fkb
COPY rules /app/rules
COPY views /app/views
# The built frontend (served by the backend via --ui-dir)
COPY --from=ui-builder /src/ui/dist /app/ui/dist

# Data and log mount points (SQLite / persisted vectors live here)
VOLUME ["/data"]
EXPOSE 5177

# Top-level --data-dir comes first, then the serve subcommand; --ui-dir makes the backend serve the SPA too
ENTRYPOINT ["/app/graphtell", "--data-dir", "/data", "serve", \
            "--bind", "0.0.0.0", "--port", "5177", "--ui-dir", "/app/ui/dist"]
