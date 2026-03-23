# Stage 1: Build
FROM rustlang/rust:nightly AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release

# Stage 2: Runtime (slim image)
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/fpga-hft-data-generator /usr/local/bin/

EXPOSE 8080

CMD ["fpga-hft-data-generator"]
