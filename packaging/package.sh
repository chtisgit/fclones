#!/bin/sh

# This script generates packages for a release and places them in target/packages/fclones-<version>.
# It runs packaging/package-internal.sh in a docker container for better reproducibility of builds.
# The Linux glibc binary is linked against an older glibc (see GLIBC_VERSION in
# package-internal.sh), so the packages are compatible with older Linux distributions.
#
# Build the image first with build-image.sh.

FCLONES_HOME=$(realpath "$(dirname "$0")/..")
echo "$FCLONES_HOME"
IMAGE="fclones-builder"
docker run --rm \
    -v "$FCLONES_HOME":/fclones \
    -u "$(id -u):$(id -g)" \
    -it $IMAGE /fclones/packaging/package-internal.sh
