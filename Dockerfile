# syntax=docker/dockerfile:1
# The vcrawl image (plan Task 15; runtime contract in infrastructure/README.md and
# deploy/compose.yaml):
#   * ENTRYPOINT ["vcrawl"]; the role is the command (`vcrawl fetcher`, `vcrawl health`, …);
#   * runs as uid 65532 (distroless `nonroot`) on a read-only root filesystem;
#   * secrets arrive as files named by VC_*_FILE (/run/secrets/*), never baked in or in env;
#   * data the roles read at runtime lives under /app (the working directory): policy/,
#     sources/, kinds.toml, schemas/, prompts/, migrations/, and the GeoNames subset of
#     crates/vc-extract/data at data/ (vc-extract's default `data/*.tsv.gz` paths).
# Base images are pinned by digest; Dependabot/Renovate moves them.
#
#   docker build -t vcrawl .        # from the repository root

FROM rust:1.99.0-slim-trixie@sha256:2752b332db73fdbb7dc576f06c82ed1f312005784ef913d7e04a28f5f55dc581 AS build
WORKDIR /src
# Optional: limit build parallelism (`--build-arg CARGO_BUILD_JOBS=2`).
ARG CARGO_BUILD_JOBS
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p vcrawl --bin vcrawl \
 && install -D -m 0755 target/release/vcrawl /out/vcrawl

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:cb87acc574abe719b6e5c547384defa2f7b1f24af9605d3f618cf852495d8be2
WORKDIR /app
COPY --from=build /out/vcrawl /usr/local/bin/vcrawl
COPY policy/ /app/policy/
COPY sources/ /app/sources/
COPY schemas/ /app/schemas/
COPY prompts/ /app/prompts/
COPY migrations/ /app/migrations/
COPY kinds.toml /app/kinds.toml
COPY crates/vc-extract/data/ /app/data/
COPY LICENSE BOT.md /app/
USER 65532:65532
ENV VC_OBS_LOG_FORMAT=json
EXPOSE 9464
ENTRYPOINT ["vcrawl"]
CMD ["--help"]
