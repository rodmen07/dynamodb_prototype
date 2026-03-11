# Multi-stage Dockerfile for dynamodb_prototype dashboard
# Builds the Rust binary in a full toolchain stage, then copies the release binary

FROM rust:1.94-bookworm AS builder
WORKDIR /app

# Copy manifest and source
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY dashboard ./dashboard

RUN apt-get update && apt-get install -y pkg-config libssl-dev ca-certificates && rm -rf /var/lib/apt/lists/*
RUN cargo build --release --bin dashboard

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app

# Copy the built binary
COPY --from=builder /app/target/release/dashboard /usr/local/bin/dashboard

EXPOSE 8080

ENV RUST_LOG=info
ENV DDB_TABLE=example_table

CMD ["/usr/local/bin/dashboard"]
