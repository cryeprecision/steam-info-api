# Build the project
FROM rustlang/rust:nightly AS builder

# Create a dummy project to cache dependencies
RUN cargo new steam-info-api --bin
WORKDIR /steam-info-api

# Copy the dependency
RUN apt update && apt install -y git pkg-config

# Copy the repository and prevent caching
ADD https://api.github.com/repos/oof-software/steam_api_concurrent/git/refs/heads/main ../steam_api_concurrent.json
RUN git clone https://github.com/oof-software/steam_api_concurrent.git ../steam_api_concurrent

# Copy the dependencies and build to cache them
COPY ./Cargo.toml ./Cargo.lock ./
RUN cargo build --release

# Copy necessary files to build the actual project
COPY ./ ./
# Prevent some caching thing idk
RUN touch ./src/main.rs
# Build the actual project
RUN cargo build --release

# Run the built binary
FROM debian:bookworm-slim

COPY --from=builder /steam-info-api/target/release/steam-info-api /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/steam-info-api"]
