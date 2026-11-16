#!/bin/bash
set -e -o pipefail
bold=$(tput bold 2>/dev/null || true)
normal=$(tput sgr0 2>/dev/null || true)

# This script generates packages for a release and places them in target/packages/fclones-<version>.
# Use package.sh to run it in the Docker image built from packaging/Dockerfile. It can also run
# directly on a Debian/Ubuntu machine with the tools installed that the Dockerfile installs.

# Oldest glibc version the Linux glibc binary (and the deb/rpm packages) must run with.
GLIBC_VERSION=${GLIBC_VERSION:-2.28}

cd "$(dirname "$0")/.."

echo "${bold}Building${normal}"
set -x
# cargo-zigbuild links against the given glibc version instead of the one of the build machine.
cargo zigbuild --release -p fclones --target "x86_64-unknown-linux-gnu.$GLIBC_VERSION"
cargo zigbuild --release -p fclones --target x86_64-unknown-linux-musl
cargo zigbuild --release -p fclones --target i686-unknown-linux-musl
cargo build --release -p fclones --target x86_64-pc-windows-gnu
set +x

GLIBC_REQUIRED=$(objdump -T target/x86_64-unknown-linux-gnu/release/fclones | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)
if [ "$(printf '%s\n' "$GLIBC_REQUIRED" "GLIBC_$GLIBC_VERSION" | sort -V | tail -1)" != "GLIBC_$GLIBC_VERSION" ]; then
    echo "The glibc binary requires $GLIBC_REQUIRED, more than GLIBC_$GLIBC_VERSION" >&2
    exit 1
fi

echo "${bold}Packaging${normal} fclones"
set -x
VERSION=$(cargo pkgid -p fclones | sed 's/.*#//')
PKG_DIR=target/packages/fclones-$VERSION
mkdir -p "$PKG_DIR"
rm -f "${PKG_DIR:?}"/*

# cargo-deb prints the path of the package it wrote.
DEB=$(cargo deb -p fclones --target x86_64-unknown-linux-gnu --no-build)
mv "$DEB" "$PKG_DIR"

# The RPM is converted from the deb via a generated spec file, so that system directories can be
# removed from %files: alien < 8.95.1 lists /usr, /usr/bin, /usr/share, ... as owned by the package,
# which conflicts with the "filesystem" package on Fedora/RHEL (newer alien versions don't).
fakeroot alien --to-rpm -g -c "$PKG_DIR"/*.deb
RPM_TREE=fclones-$VERSION
RPM_SPEC=$(ls "$RPM_TREE"/*.spec)
sed -i -E '/^%dir "\/(usr|usr\/bin|usr\/lib|usr\/lib\/\.build-id(\/[^"]*)?|usr\/share|usr\/share\/doc)\/?"$/d' "$RPM_SPEC"
# alien doesn't carry the deb's Maintainer over; use it as the RPM Packager.
PACKAGER=$(dpkg-deb -f "$PKG_DIR"/*.deb Maintainer)
(cd "$RPM_TREE" && fakeroot rpmbuild --buildroot="$(pwd)" --define "_rpmdir $(pwd)/.." \
    --define "packager $PACKAGER" \
    --define "_build_name_fmt %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm" \
    -bb --target x86_64 "$(basename "$RPM_SPEC")")
mv fclones-"$VERSION"-*.x86_64.rpm "$PKG_DIR"
rm -rf "${RPM_TREE:?}"

fakeroot alien --to-tgz -c "$PKG_DIR"/*.deb
mv "fclones-$VERSION.tgz" "$PKG_DIR/fclones-$VERSION-linux-glibc-x86_64.tar.gz"

TAR_OPTS=(--owner=0 --group=0 --numeric-owner --mode=755)
tar "${TAR_OPTS[@]}" -zcvf "$PKG_DIR/fclones-$VERSION-linux-musl-x86_64.tar.gz" target/x86_64-unknown-linux-musl/release/fclones
tar "${TAR_OPTS[@]}" -zcvf "$PKG_DIR/fclones-$VERSION-linux-musl-i686.tar.gz" target/i686-unknown-linux-musl/release/fclones
zip -j "$PKG_DIR/fclones-$VERSION-windows-x86_64.zip" target/x86_64-pc-windows-gnu/release/fclones.exe
set +x

echo "${bold}Packages${normal} in $PKG_DIR:"
ls -l "$PKG_DIR"
