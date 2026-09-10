# 多阶段构建: 静态二进制 -> distroless
FROM rust:1.95-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY protocol/ protocol/
COPY bridge/ bridge/
RUN cargo build --release --bins

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /build/target/release/sl651-bridge /usr/local/bin/sl651-bridge
COPY --from=builder /build/target/release/sim /usr/local/bin/sl651-sim
# 图片落盘目录 (挂载卷持久化)
VOLUME ["/var/lib/sl651-bridge/images"]
USER nonroot
ENTRYPOINT ["sl651-bridge"]
