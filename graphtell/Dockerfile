# GraphTell —— 单容器部署（后端 + 构建好的 Web UI）。
#
# 架构：后端 `graphtell serve` 在 5177 上同时托管 REST API 与 React SPA（--ui-dir），
# 单端口、同源、无 CORS、无需额外反向代理。
#
# 构建（默认开启 model-candle / model-ort，召回走真实 bge-m3 语义向量；权重缺失时自动退回词面）：
#   docker build -t graphtell:latest .
#
# 若想构建更小的「仅词面」镜像（不编译 candle / ort），用 build arg 关掉默认 feature：
#   docker build -t graphtell:hash --build-arg GT_FEATURES="--no-default-features" .
#
# 运行：
#   docker run -d -p 5177:5177 -v $(pwd)/data:/data graphtell:latest
#   浏览器打开 http://localhost:5177/

############################ 阶段 1：构建前端 ############################
FROM node:20-slim AS ui-builder
WORKDIR /src/ui
# 同一份 lockfile 驱动可复现安装
COPY ui/package.json ui/package-lock.json ./
RUN npm ci
COPY ui/ ./
# 同源部署：API 走相对路径，无需在构建期写死主机名
ENV VITE_API_BASE=same-origin
RUN npm run build

############################ 阶段 2：构建后端 ############################
FROM rust:1.83-slim AS rust-builder
# model-candle / model-ort 的本地构建依赖
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config cmake \
        libssl-dev clang libclang-dev protobuf-compiler \
        libsqlite3-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# 用 build arg 控制 feature（默认空 = 使用 Cargo.toml 里的默认 feature：candle + ort）
ARG GT_FEATURES=""
RUN cargo build --release ${GT_FEATURES} -p gt-app

############################ 阶段 3：运行镜像 ############################
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        libssl3 libgcc-s1 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

# 后端二进制
COPY --from=rust-builder /src/target/release/graphtell /app/graphtell
# 内置资产（FKB / 规则 / 视角声明）：二进制按「可执行文件相邻目录」解析
COPY fkb /app/fkb
COPY rules /app/rules
COPY views /app/views
# 构建好的前端（由后端 --ui-dir 托管）
COPY --from=ui-builder /src/ui/dist /app/ui/dist

# 数据与日志挂载点（SQLite / 落盘向量都在这里）
VOLUME ["/data"]
EXPOSE 5177

# 顶层 --data-dir 在前、serve 子命令在后；--ui-dir 让后端顺带托管 SPA
ENTRYPOINT ["/app/graphtell", "--data-dir", "/data", "serve", \
            "--bind", "0.0.0.0", "--port", "5177", "--ui-dir", "/app/ui/dist"]
