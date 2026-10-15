# Container image for the Rust harness.
#
# The binary is built in the image (no cargo in the runtime layer is needed
# beyond the build); entrypoint is `little_helper`. The image MUST be run with
# explicit state/workspace envs (run.sh does this):
#
#   -e HARNESS_STATE=/state -e HARNESS_WORKSPACE=/workspace
#
# The binary refuses to start without both (dev-safety, AGENT.md rule 2), so
# a bare `podman run little_helper` fails loudly instead of guessing at paths.

FROM registry.fedoraproject.org/fedora:44

# Toolchain + the runtime tools the agent uses (git, ps, podman for nested
# work; nodejs/npm kept for parity with the reference image).
RUN dnf install -y \
        git \
        nodejs npm \
        rust cargo \
        gcc gcc-c++ clang make cmake \
        ps \
        podman \
		postgresql \
    && dnf clean all

# The agent auto-commits workspace + state repos; give git a stable identity
# so commits never fail inside the container.
RUN git config --global user.name "harness" \
 && git config --global user.email "harness@localhost" \
 && git config --global init.defaultBranch main

WORKDIR /app

# Build the release binary.
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
RUN cargo build --release --locked \
 && install -m 0755 target/release/little_helper /usr/local/bin/little_helper

# The mounts (see run.sh). /workspace holds the project, /state the config +
# session logs + git history.
WORKDIR /workspace

ENTRYPOINT ["little_helper"]
