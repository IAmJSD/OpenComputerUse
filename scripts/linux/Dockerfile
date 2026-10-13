# A Linux box to build and exercise the Linux backend in, X11 and Wayland:
#   docker build -f scripts/linux/Dockerfile -t ocu-linux . && docker run --rm ocu-linux
FROM rust:1-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends xvfb x11-apps python3 \
    python3-websocket chromium foot fonts-dejavu-core xkb-data libpixman-1-dev \
    libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libxcb1-dev \
    libfontconfig-dev libfreetype-dev libvulkan-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --bin opencomputeruse
CMD ["sh", "-c", "python3 scripts/linux/smoke.py && python3 scripts/linux/wayland_smoke.py"]
