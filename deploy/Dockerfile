FROM rust:1-bookworm AS build

RUN apt-get update \
    && apt-get install --yes --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY backend ./backend
RUN cargo build --locked --release --package chuanshanjia-backend --bin chuanshanjia-server

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 chuanshanjia
WORKDIR /app
COPY --from=build /app/target/release/chuanshanjia-server /usr/local/bin/chuanshanjia-server
USER chuanshanjia
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/chuanshanjia-server"]
