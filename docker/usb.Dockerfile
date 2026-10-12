# syntax=docker/dockerfile:1
# check=skip=SecretsUsedInArgOrEnv
FROM rust:1.99-slim-trixie@sha256:2752b332db73fdbb7dc576f06c82ed1f312005784ef913d7e04a28f5f55dc581 AS build
# Release key for the stick's self-update, hex; empty keeps the one in proto/src/release.rs.
ARG MEMBURN_RELEASE_KEY=
RUN rustup target add x86_64-unknown-linux-musl x86_64-unknown-uefi x86_64-pc-windows-gnu \
 && apt-get update && apt-get install -y --no-install-recommends gcc-mingw-w64-x86-64 musl-tools \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    cargo build --release --locked --target x86_64-unknown-linux-musl -p memburn -p memburn-init \
 && cargo build --release --locked --target x86_64-unknown-uefi -p memburn-uefi \
 && cargo build --release --locked --target x86_64-pc-windows-gnu -p memburn \
 && mkdir /bin-out \
 && cp target/x86_64-unknown-linux-musl/release/memburn target/x86_64-unknown-linux-musl/release/memburn-init \
       target/x86_64-unknown-uefi/release/memburn-uefi.efi target/x86_64-pc-windows-gnu/release/memburn.exe /bin-out/

FROM ubuntu:26.04@sha256:f144425ff09be612d6d9ad965196e9cdc23dae1f42110a8a11a3e9a8198759f7 AS stick
ARG KERNEL=7.0.0-38-generic
ARG KERNEL_PKG=7.0.0-38.38
ARG SHIM=1.59+15.8-0ubuntu2
ARG GRUB=1.215+2.14-2ubuntu1
ARG BUSYBOX=1:1.37.0-7ubuntu1
ARG VERSION
RUN apt-get update && apt-get install -y --no-install-recommends kmod cpio zstd mtools fdisk \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /bin-out /in
COPY scripts/usb /usb
COPY uefi/memburn.cfg.example /usb/
RUN --mount=type=cache,target=/var/lib/apt/lists /usb/assemble.sh

FROM scratch
COPY --from=stick /out/ /
