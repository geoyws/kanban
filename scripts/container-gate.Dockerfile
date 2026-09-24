# The Linux image `scripts/container-gate.sh` runs the release gate in.
#
# Kanban deploys to Linux, so its gate runs on Linux too: a macOS-only pass
# proves nothing about the served host, and the first Linux run (2026-09-24,
# t-a3928b36) found a test that had never run and a delivery race that
# macOS never showed. The container runs on the build machine's native
# architecture; the release artifact itself is still built by
# `scripts/hig-release.sh`, not here.
#
# Base: rust:1.95-bookworm pinned by manifest digest (kanban board
# checkpoint seq 569, t-a3928b36). Added on top, each because a gate step
# needs it:
#   - chromium + fonts: the e2e browser suite (KANBAN_CHROME=/usr/bin/chromium)
#   - jq, git, curl, unzip: script steps and the bun download
#   - rustfmt, clippy: steps 2 and 3
#   - bun 1.4.2: the web bundle gate, pinned to the measured version
#   - a passwd entry for the host uid: the authz matrix resolves the caller
#     with `id -un`, exactly as the guard does, so an unnamed uid cannot run it
FROM rust@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        jq chromium fonts-liberation fonts-dejavu-core \
        git ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/*

RUN /usr/local/cargo/bin/rustup component add rustfmt clippy

ARG TARGETARCH
RUN case "${TARGETARCH:-$(dpkg --print-architecture)}" in \
        arm64) bun_arch=aarch64 ;; \
        amd64) bun_arch=x64 ;; \
        *) echo "no bun build for ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && curl -fsSL "https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/bun-linux-${bun_arch}.zip" \
        -o /tmp/bun.zip \
    && unzip -q /tmp/bun.zip -d /opt \
    && install -m 0755 "/opt/bun-linux-${bun_arch}/bun" /usr/local/bin/bun \
    && rm -rf /tmp/bun.zip "/opt/bun-linux-${bun_arch}" \
    && bun --version

# Last, so a different host uid rebuilds only this layer.
ARG GATE_UID=501
ARG GATE_GID=20
RUN (getent group "${GATE_GID}" >/dev/null || groupadd -g "${GATE_GID}" gate) \
    && useradd -u "${GATE_UID}" -g "${GATE_GID}" -M -s /bin/bash gate \
    && setpriv --reuid "${GATE_UID}" --regid "${GATE_GID}" --clear-groups id -un | grep -qx gate
