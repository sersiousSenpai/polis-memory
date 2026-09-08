# The org node (plan §8: Docker is for org-node operators only — an
# individual runs the binary). A static musl `polis` on distroless, non-root,
# one volume. Build: `docker build -t polis-memory .`
# Run:   docker run -d --name polis-org -p 7677:7677 -v polis-data:/data \
#          -e POLIS_HOME=/data polis-memory
# The token: the node refuses a non-loopback listen without one. Put it at
# /data/token (0600) before the first start, or let `polis init` create it:
#   docker run --rm -v polis-data:/data -e POLIS_HOME=/data polis-memory init --org firm
#
# `serve --org` is Session E4's flag; until it lands the same image serves a
# plain daemon (`serve --listen … --token-file …`) — the CMD below is the
# org node's contract, overridable per run.

FROM rust:1-alpine AS build
RUN apk add --no-cache build-base musl-dev pkgconfig
WORKDIR /src
COPY . .
# The release profile (thin LTO, one codegen unit, stripped) + the `cli`
# feature. `apple` inside `cli` is target-gated and compiles to nothing here.
RUN cargo build --release --features cli -p polis-memory --locked \
 && strip -s target/release/polis 2>/dev/null || true \
 && ls -la target/release/polis \
 && mkdir -p /data

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/polis /usr/local/bin/polis
# The home, owned by the non-root user the node runs as: a named volume
# created from this image inherits the directory and its owner, so the
# first `init` can write.
COPY --from=build --chown=nonroot:nonroot /data /data
ENV POLIS_HOME=/data
VOLUME ["/data"]
EXPOSE 7677
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/polis"]
CMD ["serve", "--org", "--listen", "0.0.0.0:7677", "--token-file", "/data/token"]
