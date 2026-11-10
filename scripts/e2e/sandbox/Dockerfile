# The OpenShell sandbox image for the e2e run: an Ubuntu userspace with the tools
# the scripts need and the scripts themselves. Built with context scripts/e2e.
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
        bash ca-certificates coreutils curl grep gzip procps python3 sed tar minisign \
        libxcb1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libfontconfig1 libx11-xcb1 libxcb-xkb1 libegl1 libgl1 libvulkan1 \
    && rm -rf /var/lib/apt/lists/*

# A non-root identity, as OpenShell requires; the workspace is /sandbox.
RUN groupadd --gid 1500 app \
    && useradd --uid 1500 --gid app --create-home app \
    && install -d -o app -g app /sandbox
WORKDIR /sandbox
COPY --chown=app:app . /sandbox/e2e
USER app
