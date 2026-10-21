# Build images for esp-xip-modules (podman or docker).
#
# The official Espressif image already has ESP-IDF v6.0.2 and the esp32 / esp32s3 / esp32c6 toolchains,
# so nothing is downloaded at build time.
#
#   podman build --target build-env -t esp-xip-modules-build .
#   podman run --rm -v "$PWD":/src:Z -w /src esp-xip-modules-build scripts/build.sh esp32c6
#
# Only the binaries, into ./target/out:
#   podman build --target bin --output type=local,dest=target/out .
ARG IDF_IMAGE=docker.io/espressif/idf:v6.0.2

FROM ${IDF_IMAGE} AS build-env
WORKDIR /src
# The image entrypoint sources ESP-IDF's export.sh, then runs the command.

FROM build-env AS build
COPY . /src
RUN scripts/build.sh all

FROM scratch AS bin
COPY --from=build /src/target/out/ /
