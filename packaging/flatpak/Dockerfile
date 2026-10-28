# Packaging only: no runtime sandbox or privileged Docker container is needed.
FROM ubuntu:24.04
RUN apt-get update -q && apt-get install -qy --no-install-recommends \
    flatpak binutils squashfs-tools ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /src
ENTRYPOINT ["scripts/package-flatpak.sh"]
