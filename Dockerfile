# syntax=docker/dockerfile-upstream:experimental

FROM ubuntu:18.04 as build

RUN apt-get update -qq && apt-get install -y \
    git \
    cmake \
    g++ \
    pkg-config \
    libssl-dev \
    curl \
    llvm \
    clang \
    && rm -rf /var/lib/apt/lists/*

COPY ./rust-toolchain.toml /tmp/rust-toolchain.toml

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH

RUN curl https://sh.rustup.rs -sSf | \
    sh -s -- -y --no-modify-path --default-toolchain none

VOLUME [ /near ]
WORKDIR /near
COPY . .

ENV RUST_LOG="debug,actix_web=info"
ENV PORTABLE=ON
ARG make_target=neard
RUN make CARGO_TARGET_DIR=/tmp/target \
         "${make_target:?make_target not set}"

# Actual image
FROM ubuntu:18.04

EXPOSE 3030 24567

RUN apt-get update -qq && apt-get install -y \
    libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ENV RUST_LOG="debug,actix_web=info"

# COPY ./tools/protocol-sandbox/docker/run_docker_local_test.sh /usr/local/bin/run.sh
COPY --from=build /tmp/target/release/neard /usr/local/bin/

CMD ["/usr/local/bin/run.sh"]