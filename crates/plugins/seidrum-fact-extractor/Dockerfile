FROM rust:1.85-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release --package seidrum-fact-extractor

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/seidrum-fact-extractor /usr/local/bin/
ENTRYPOINT ["seidrum-fact-extractor"]
