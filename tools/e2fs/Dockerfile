# Reference e2fsprogs used as a test oracle (mke2fs, e2fsck, debugfs, dumpe2fs).
FROM debian:trixie-slim
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends e2fsprogs gdisk xxd && rm -rf /var/lib/apt/lists/*
WORKDIR /work
