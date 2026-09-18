# syntax=docker/dockerfile:1.4.0
# BOTH STAGES ARE BOOKWORM, and they had to move together. They were bullseye,
# inherited from upstream, and the runtime stage installed a TLS library that
# bullseye no longer carries. The first CI build failed in seven seconds on that
# apt-get with exit 100, which reads like a broken Dockerfile rather than a
# distribution that aged out from under it.
#
# Moving only the runtime stage would give a binary linked against the old
# library on an image that has only the new one, and that failure does not
# surface until a customer starts the container.
FROM --platform=linux/amd64 rust:1.84-slim-bookworm as builder

# `perl` in full, not the perl-base the slim image ships. openssl-sys builds
# OpenSSL from source here, and its Configure script is perl that needs modules
# perl-base does not carry. Without it the build fails at
# "'perl' reported failure with exit status: 2", which names perl and says
# nothing about OpenSSL, so it reads like a broken toolchain.
RUN apt-get -qq update && apt-get install -qq -y ca-certificates libssl-dev protobuf-compiler pkg-config libudev-dev zlib1g-dev llvm clang cmake make libprotobuf-dev g++ perl
RUN rustup component add rustfmt && update-ca-certificates

ENV HOME=/home/root
WORKDIR $HOME/app
COPY . .

# with buildkit, you need to copy the binary to the main folder
# w/o buildkit, you can remove the cp
RUN --mount=type=cache,mode=0777,target=/home/root/app/target \
    --mount=type=cache,mode=0777,target=/usr/local/cargo/registry \
    --mount=type=cache,mode=0777,target=/usr/local/cargo/git \
    cargo build --release && cp target/release/localshred-* ./

################################################################################
FROM --platform=linux/amd64 debian:bookworm-slim as base_image
# keep iproute2 for multicast route parsing
RUN apt-get -qq update && apt-get install -qq -y ca-certificates libssl3 iproute2 && rm -rf /var/lib/apt/lists/*

################################################################################
FROM base_image as localshred_lite_proxy
ENV APP="localshred-lite-proxy"

WORKDIR /app
# with buildkit, the binary is placed in the git root folder
# w/o buildkit, the binary will be in target/release
COPY --from=builder /home/root/app/${APP} ./
ENTRYPOINT ["/app/localshred-lite-proxy"]
