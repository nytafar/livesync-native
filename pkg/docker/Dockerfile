# syntax=docker/dockerfile:1
#
# lsync serve in a FROM scratch image: the static musl binary (it bundles its
# CA roots) and nothing else, running as a user of its own (uid and gid
# 10001), as a system service with its config, state and vaults as volumes.
#
# The build context holds the release's binaries as <arch>/lsync (amd64,
# arm64); pkg/docker/build.sh lays it out from a release's archives, checked
# against its SHA256SUMS. See pkg/README.md.
#
#   docker run -d --name lsync \
#     -v ./config:/config -v ./state:/state -v ~/Notes:/vaults/notes \
#     ghcr.io/nytafar/lsync
#
# /config  lsync.toml and secrets.toml (`docker exec lsync lsync account add ...`)
# /state   each vault's index, and the control socket in /state/.control
# /vaults  the vault folders (`--folder /vaults/notes`)
#
# All three must be writable by uid 10001, or run the container with
# --user UID:GID of the folders' owner.

FROM --platform=$BUILDPLATFORM busybox:1.37 AS layout
ARG TARGETARCH
COPY --chmod=0755 ${TARGETARCH}/lsync /out/usr/local/bin/lsync
RUN mkdir -p /out/config /out/state /out/vaults /out/tmp \
 && chown 10001:10001 /out/config /out/state /out/vaults \
 && chmod 0700 /out/config /out/state \
 && chmod 1777 /out/tmp

FROM scratch
COPY --from=layout /out/ /
USER 10001:10001
# `lsync --system`: the system service's config and state folders, which
# CONFIGURATION_DIRECTORY and STATE_DIRECTORY move to the volumes. Every
# lsync run in the container (docker exec) works with the same service.
ENV LSYNC_SYSTEM=true \
    CONFIGURATION_DIRECTORY=/config \
    STATE_DIRECTORY=/state \
    LSYNC_RUNTIME_DIR=/state/.control \
    HOME=/vaults \
    TMPDIR=/tmp \
    NO_COLOR=1
VOLUME ["/config", "/state"]
WORKDIR /vaults
ENTRYPOINT ["/usr/local/bin/lsync"]
CMD ["serve"]
