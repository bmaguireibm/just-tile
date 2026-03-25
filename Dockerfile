# Stage 1: Builder
# Using bookworm-slim as it's a reliable base for cross-compilation
FROM rust:1.85-slim-bookworm AS builder

# Install musl development tools
RUN apt-get update && apt-get install -y \
    musl-tools \
    musl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Add the musl target for static linking
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app
COPY . .

# Build the application statically linked for musl
# We use --release for optimization and explicitly target musl
RUN cargo build --release --target x86_64-unknown-linux-musl

# Stage 2: Final Image
# Starting from scratch results in the smallest possible image (~10MB)
FROM scratch

# Import CA certificates from the builder stage
# Required for HTTPS requests to S3 (since we use rustls)
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

# Copy the statically linked binary
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/just-tile /just-tile

# Expose the default port
EXPOSE 3000

# Run the binary
ENTRYPOINT ["/just-tile"]
