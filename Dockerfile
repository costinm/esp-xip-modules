# Build images for esp-xip-modules (podman or docker).
#
# The official Espressif image already has ESP-IDF v6.0.2 and the esp32 / esp32s3 / esp32c6 toolchains,
# so nothing is downloaded at build time.
#
#   podman build --build-context loader=../esp-wifi-loader --target build-env -t esp-xip-modules-build .
#   podman run --rm -v "$PWD":/src:Z -w /src esp-xip-modules-build scripts/build.sh esp32c6
#
# Only the binaries, into ./target/out:
#   podman build --build-context loader=../esp-wifi-loader --target bin --output type=local,dest=target/out .
#
# Signing is esp-wifi-loader's: its tools/ come in as the named build context `loader` (no copy of them lives here),
# and its Dockerfile.sign is the small signing image for releases (docs/releasing.md).
ARG IDF_IMAGE=docker.io/espressif/idf:v6.0.2

FROM ${IDF_IMAGE} AS build-env
# openssl: the signing tool's fallback and the host tests.
RUN apt-get update && apt-get install -y --no-install-recommends openssl \
    && rm -rf /var/lib/apt/lists/*
# the loader checkout: its component verifies uploads, its tools sign and flash
COPY --from=loader . /loader
ENV ESP_WIFI_LOADER_DIR=/loader
WORKDIR /src
# The image entrypoint sources ESP-IDF's export.sh, then runs the command.

FROM build-env AS build
COPY . /src
RUN scripts/build.sh all

FROM scratch AS bin
COPY --from=build /src/target/out/ /
