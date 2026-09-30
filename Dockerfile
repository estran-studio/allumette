# Signaling server as a docker image
#
# to build, run `docker build -f Dockerfile` from root of the
# repository

FROM rust:1.90-slim-bookworm AS builder

WORKDIR /usr/src/allumette_server/

COPY README.md /usr/src/README.md
COPY ./Cargo.toml /usr/src/allumette_server/Cargo.toml
COPY ./src /usr/src/allumette_server/src

RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/allumette_server/target/release/allumette_server /usr/local/bin/allumette_server
CMD ["allumette_server"]
