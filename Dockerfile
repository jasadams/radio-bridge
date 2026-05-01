FROM rust:1.94-bookworm AS builder
RUN rustup target add x86_64-unknown-linux-musl && \
    apt-get update && apt-get install -y --no-install-recommends musl-tools && \
    rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY crates/ crates/
COPY providers/ providers/
ENV CC_x86_64_unknown_linux_musl=musl-gcc
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/radio-bridge /radio-bridge
EXPOSE 8000
ENTRYPOINT ["/radio-bridge"]
