FROM rust:1.90-slim-bookworm AS builder

WORKDIR /app
COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/health-service /usr/local/bin/health-service

ENV HOST=0.0.0.0
ENV PORT=3000

EXPOSE 3000

CMD ["health-service"]
