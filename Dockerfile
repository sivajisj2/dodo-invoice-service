FROM rust:1.90-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY openapi.json ./openapi.json
RUN cargo build --release --bins

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/dodo-invoice-service /usr/local/bin/dodo-invoice-service
COPY --from=builder /app/target/release/mock-psp /usr/local/bin/mock-psp
COPY migrations ./migrations
ENV RUST_LOG=info
CMD ["dodo-invoice-service"]
