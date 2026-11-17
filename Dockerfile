# Build the web UI, embed it into the release backend, and ship only the backend binary.
#
#   docker compose up -d        # from the repository root; open http://localhost:8787
#
# The image listens on all interfaces inside the container (MOCHIPREP_HOST=0.0.0.0). compose.yaml
# publishes it on 127.0.0.1 only, so the API is still reachable from this machine alone unless you
# change the port mapping on purpose.

FROM node:22-bookworm-slim AS web
# pnpm needs no install scripts; skipping them keeps a compromised package from running code here.
RUN npm install -g --ignore-scripts pnpm@11.24.0
WORKDIR /src
COPY . .
RUN pnpm install --frozen-lockfile && pnpm build

FROM rust:1.99-slim-bookworm AS build
WORKDIR /src
COPY . .
# rust-embed reads apps/web/dist when mochi-server compiles, so the UI must exist first.
COPY --from=web /src/apps/web/dist apps/web/dist
RUN cargo build --locked --release -p mochi-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home-dir /data --shell /usr/sbin/nologin mochiprep \
 && mkdir /data \
 && chown mochiprep:mochiprep /data
COPY --from=build /src/target/release/mochi-server /usr/local/bin/mochi-server

ENV MOCHIPREP_HOST=0.0.0.0 \
    MOCHIPREP_PORT=8787 \
    MOCHIPREP_DATA_DIR=/data
VOLUME ["/data"]
EXPOSE 8787
USER mochiprep
ENTRYPOINT ["mochi-server"]
