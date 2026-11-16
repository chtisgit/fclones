#!/bin/sh

# Builds the docker image used for generating the fclones packages (see package.sh)
IMAGE="fclones-builder"
docker build -t $IMAGE "$(realpath "$(dirname "$0")")"
