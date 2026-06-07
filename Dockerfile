FROM rust:1.93-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release 2>/dev/null || true
COPY src/ ./src/
RUN touch src/main.rs && cargo build --release

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y ripgrep && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/chatdex-mcp /usr/local/bin/
EXPOSE 6967
CMD ["chatdex-mcp"]
