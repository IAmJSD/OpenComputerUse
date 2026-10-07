# A Linux box to build and exercise the Linux backend in:
#   docker build -f scripts/linux/Dockerfile -t ocu-linux . && docker run --rm ocu-linux
FROM rust:1-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends xvfb x11-apps python3 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --bin opencomputeruse
CMD ["python3", "scripts/linux/smoke.py"]
