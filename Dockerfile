# The image is the TOOLCHAIN ONLY.
#
# No source is baked in. `docker compose run` reuses an existing image unless `--build` is
# passed, so an image containing sources silently re-runs the *previous* stage's binaries
# and reports green with the new tests simply absent — a failure that is near-undetectable,
# because the suite passes and only the test count betrays it. The working tree is mounted
# at /app instead, so a run always executes the tree on disk.
#
# The dependency-cache trick of copying dummy sources, building, then deleting them is also
# avoided: `rm -rf crates` leaves fingerprints in the target directory, and cargo's mtime
# check then links the real sources against a stale empty library. Only `cargo fetch` is
# cached — the manifests are copied into a scratch directory, the registry is populated,
# and the scratch directory is removed. Nothing is compiled here, so no fingerprint exists
# to go stale.
FROM rust:1.91-slim

RUN rustup component add clippy

# Build artifacts go to a named volume mounted here, never into the mounted working tree:
# host and container target directories would otherwise fight over the same fingerprints.
ENV CARGO_TARGET_DIR=/target

WORKDIR /fetch
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml crates/core/
COPY crates/chain/Cargo.toml crates/chain/
COPY crates/runtime/Cargo.toml crates/runtime/
COPY crates/scenarios/Cargo.toml crates/scenarios/
# Cargo refuses to resolve a workspace whose members declare no target, so each member gets
# an empty one. This is not the dummy-source cache trick and does not share its failure
# mode: nothing is compiled here, so no fingerprint is written to $CARGO_TARGET_DIR for a
# later real build to match against. The empty files exist only inside /fetch, and /fetch is
# gone by the end of the same layer. What survives is the registry under $CARGO_HOME.
RUN mkdir -p crates/core/src crates/chain/src crates/runtime/src crates/scenarios/src \
    && touch crates/core/src/lib.rs crates/chain/src/lib.rs crates/runtime/src/lib.rs \
    && echo 'fn main() {}' > crates/scenarios/src/main.rs \
    && cargo fetch --locked \
    && rm -rf /fetch

WORKDIR /app
