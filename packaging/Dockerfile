# Image for building the fclones release packages (see package.sh).
#
# The Linux glibc binary is linked with cargo-zigbuild against glibc 2.28, so it runs on the same
# distributions as the packages built in the earlier Debian 10 (buster) image, although this image
# is based on a current Debian. Linking with zig also works when the image runs on arm64 hosts
# (e.g. Apple Silicon).
FROM rust:1.100.0-trixie

RUN apt-get update && apt-get install -y --no-install-recommends \
        alien \
        binutils \
        fakeroot \
        gcc-mingw-w64-x86-64 \
        rpm \
        xz-utils \
        zip \
    && rm -rf /var/lib/apt/lists/*

# Zig is only used as the linker by cargo-zigbuild. Checksums from https://ziglang.org/download/index.json
ARG ZIG_VERSION=0.13.0
RUN set -e; \
    case "$(uname -m)" in \
        x86_64)  ZIG_ARCH=x86_64;  ZIG_SHA256=d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea ;; \
        aarch64) ZIG_ARCH=aarch64; ZIG_SHA256=041ac42323837eb5624068acd8b00cd5777dac4cf91179e8dad7a7e90dd0c556 ;; \
        *) echo "unsupported architecture $(uname -m)"; exit 1 ;; \
    esac; \
    curl -sSfL -o /tmp/zig.tar.xz "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-${ZIG_ARCH}-${ZIG_VERSION}.tar.xz"; \
    echo "${ZIG_SHA256}  /tmp/zig.tar.xz" | sha256sum -c -; \
    tar -xJf /tmp/zig.tar.xz -C /opt; \
    ln -s "/opt/zig-linux-${ZIG_ARCH}-${ZIG_VERSION}/zig" /usr/local/bin/zig; \
    rm /tmp/zig.tar.xz

RUN rustup target add \
        x86_64-unknown-linux-gnu \
        x86_64-unknown-linux-musl \
        i686-unknown-linux-musl \
        x86_64-pc-windows-gnu
RUN cargo install --locked cargo-zigbuild@0.24.1 cargo-deb@3.8.0

# package.sh runs the container with the caller's uid, which has no home directory in the image.
# cargo, zig and rpmbuild need writable cache and work directories.
RUN mkdir -m 1777 /home/builder && chmod -R a+rwX /usr/local/cargo
ENV HOME=/home/builder
