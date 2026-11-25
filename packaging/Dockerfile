FROM ubuntu:22.04 AS build
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential binutils git patch curl ca-certificates ninja-build pkg-config python3 python3-venv python3-pip python3-tomli libglib2.0-dev libpixman-1-dev libgcrypt20-dev patchelf \
    libx11-6 libx11-xcb1 libxcursor1 libxi6 libxrandr2 libxcb1 libxcb-render0 libxcb-shape0 libxcb-xfixes0 libxcb-xkb1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libwayland-cursor0 libwayland-egl1 libasound2 \
    && rm -rf /var/lib/apt/lists/*
ENV CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml build.py LICENSE README.md ./
COPY src src
COPY crates crates
COPY qemu qemu
COPY packaging packaging
COPY LICENSES LICENSES
ARG JOBS=4
RUN python3 build.py --portable --jobs "$JOBS"
FROM scratch AS artifact
COPY --from=build /src/dist/ /
