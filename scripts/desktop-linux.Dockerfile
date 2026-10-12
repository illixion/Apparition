# Builds the Linux desktop app (Tauri, WebKitGTK 4.1) into a .deb and an AppImage, for
# scripts/package-release.sh --linux. Ubuntu 22.04, so the app needs glibc 2.35 or newer: a
# glibc program runs only on its builder's glibc or later, and WebKitGTK can't be linked
# statically the way the command line is.
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update -q && apt-get install -qy --no-install-recommends \
      build-essential ca-certificates curl file pkg-config xdg-utils \
      libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain 1.97.0 \
    && cargo install -q tauri-cli --version '^2' --locked \
    && rm -rf /usr/local/cargo/registry
# linuxdeploy, which bundles the AppImage, is itself an AppImage; there's no FUSE in a container.
ENV APPIMAGE_EXTRACT_AND_RUN=1
