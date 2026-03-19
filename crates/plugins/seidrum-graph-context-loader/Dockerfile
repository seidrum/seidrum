FROM rust:1.85-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release --package seidrum-graph-context-loader

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/seidrum-graph-context-loader /usr/local/bin/
ENTRYPOINT ["seidrum-graph-context-loader"]
