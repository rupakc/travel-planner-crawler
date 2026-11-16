# syntax=docker/dockerfile:1
# vc-e2e:local, the one image of the E2E stack (plan Task 16): vcrawl, fake_web,
# fake_gemini and e2e-runner. Test-only; never deployed.
#
#   docker build -f e2e/Dockerfile -t vc-e2e:local .                      # compiles inside
#   docker build -f e2e/Dockerfile --build-arg BINS=prebuilt -t vc-e2e:local .
#                                   # CI: Linux binaries already built into e2e/.bin/
ARG BINS=build

FROM rust:1.99.0-slim-trixie AS build
WORKDIR /src
COPY . .
ENV CARGO_TARGET_DIR=/src/target
# Optional: limit build parallelism (shared machines).
ARG CARGO_BUILD_JOBS
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build ${CARGO_BUILD_JOBS:+-j $CARGO_BUILD_JOBS} --release --locked -p vcrawl -p testkit --bin vcrawl --bin fake_web --bin fake_gemini \
 && cp Cargo.lock e2e/runner/Cargo.lock \
 && cargo build ${CARGO_BUILD_JOBS:+-j $CARGO_BUILD_JOBS} --release --manifest-path e2e/runner/Cargo.toml \
 && mkdir -p /out \
 && cp target/release/vcrawl target/release/fake_web target/release/fake_gemini \
       target/release/e2e-runner /out/

FROM scratch AS bins-build
COPY --from=build /out/ /

FROM scratch AS bins-prebuilt
COPY e2e/.bin/vcrawl e2e/.bin/fake_web e2e/.bin/fake_gemini e2e/.bin/e2e-runner /

FROM bins-${BINS} AS bins

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=bins / /usr/local/bin/
USER 65532:65532
