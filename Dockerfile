ARG DEBIAN_BASE_IMAGE=debian:stable-slim@sha256:328d16499860ae6cb9b345e2e4cebca08c2a36e4f7278482c7bd1f39d71e5bfd
ARG RUST_BUILDER_IMAGE=rust:1.97.1-slim-bookworm@sha256:99e09cb2284e2ddbb73a995deee3e91783fd04d177602ccf6eab326d778ee777

FROM ${RUST_BUILDER_IMAGE} AS rust-builder
ARG DEBIAN_SNAPSHOT=20260719T000000Z
ARG RUST_BUILDER_DEBIAN_SUITE=bookworm
WORKDIR /src
COPY container/packages/builder.txt /tmp/xenoteer-builder-packages.txt
RUN rm -f /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/" \
      "Suites: ${RUST_BUILDER_DEBIAN_SUITE} ${RUST_BUILDER_DEBIAN_SUITE}-updates" \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      '' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}/" \
      "Suites: ${RUST_BUILDER_DEBIAN_SUITE}-security" \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      > /etc/apt/sources.list.d/xenoteer.sources \
    && apt-get update \
    && sed '/^#/d; /^$/d' /tmp/xenoteer-builder-packages.txt \
      | xargs -r apt-get install -y --no-install-recommends \
    && rm -rf /var/lib/apt/lists/* /tmp/xenoteer-builder-packages.txt
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ crates/
COPY scripts/licenses/generate-cargo-manifest.sh /usr/local/bin/generate-cargo-manifest
RUN chmod 0755 /usr/local/bin/generate-cargo-manifest \
    && cargo build --locked --release --bin xenoteerd \
    && /usr/local/bin/generate-cargo-manifest \
      /src \
      /src/target/release/xenoteerd \
      /src/target/release/cargo-components.tsv \
      /src/target/release/cargo-components.spdx.json

FROM ${DEBIAN_BASE_IMAGE} AS s6-overlay
ARG DEBIAN_SNAPSHOT=20260719T000000Z
ARG S6_OVERLAY_VERSION=3.2.2.0
ARG S6_OVERLAY_ARCH=x86_64
ARG S6_OVERLAY_NOARCH_SHA256=85848f6baab49fb7832a5557644c73c066899ed458dd1601035cf18e7c759f26
ARG S6_OVERLAY_ARCH_SHA256=5a09e2f1878dc5f7f0211dd7bafed3eee1afe4f813e872fff2ab1957f266c7c0
ARG S6_OVERLAY_COPYING_SHA256=7184c7d1dae02fc4a23e0d2cda2c8a107ba08fbc0158bc25f4d0f404941780db
COPY scripts/licenses/generate-s6-manifest.sh /usr/local/bin/generate-s6-manifest
RUN rm -f /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/" \
      'Suites: stable stable-updates' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      '' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}/" \
      'Suites: stable-security' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      > /etc/apt/sources.list.d/xenoteer.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /tmp/s6 /s6-root \
    && curl --fail --location --silent --show-error \
      "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz" \
      --output /tmp/s6/noarch.tar.xz \
    && curl --fail --location --silent --show-error \
      "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-${S6_OVERLAY_ARCH}.tar.xz" \
      --output /tmp/s6/arch.tar.xz \
    && curl --fail --location --silent --show-error \
      "https://raw.githubusercontent.com/just-containers/s6-overlay/v${S6_OVERLAY_VERSION}/COPYING" \
      --output /tmp/s6-overlay-COPYING \
    && printf '%s  %s\n' "$S6_OVERLAY_NOARCH_SHA256" /tmp/s6/noarch.tar.xz \
      | sha256sum --check --strict - \
    && printf '%s  %s\n' "$S6_OVERLAY_ARCH_SHA256" /tmp/s6/arch.tar.xz \
      | sha256sum --check --strict - \
    && printf '%s  %s\n' "$S6_OVERLAY_COPYING_SHA256" /tmp/s6-overlay-COPYING \
      | sha256sum --check --strict - \
    && tar -C /s6-root -Jxpf /tmp/s6/noarch.tar.xz \
    && tar -C /s6-root -Jxpf /tmp/s6/arch.tar.xz \
    && chmod 0755 /usr/local/bin/generate-s6-manifest \
    && /usr/local/bin/generate-s6-manifest /s6-root /tmp/s6-overlay-files.tsv \
    && rm -rf /tmp/s6

FROM ${DEBIAN_BASE_IMAGE} AS runtime
ARG DEBIAN_BASE_DIGEST=sha256:328d16499860ae6cb9b345e2e4cebca08c2a36e4f7278482c7bd1f39d71e5bfd
ARG DEBIAN_SNAPSHOT=20260719T000000Z
ARG XENOTEER_VERSION=0.1.0-dev
ARG XENOTEER_REVISION=unknown
ARG XENOTEER_CREATED=1970-01-01T00:00:00Z
LABEL org.opencontainers.image.title="Xenoteer" \
      org.opencontainers.image.description="Bot-controlled isolated X11 Linux desktop" \
      org.opencontainers.image.source="https://github.com/th317erd/xenoteer" \
      org.opencontainers.image.documentation="https://github.com/th317erd/xenoteer/blob/main/container/README.md" \
      org.opencontainers.image.version="$XENOTEER_VERSION" \
      org.opencontainers.image.revision="$XENOTEER_REVISION" \
      org.opencontainers.image.created="$XENOTEER_CREATED" \
      org.opencontainers.image.licenses="NOASSERTION" \
      com.aeor.xenoteer.first-party-license="BUSL-1.1" \
      com.aeor.xenoteer.base.digest="$DEBIAN_BASE_DIGEST" \
      com.aeor.xenoteer.debian.snapshot="$DEBIAN_SNAPSHOT" \
      com.aeor.xenoteer.protocol="v1" \
      com.aeor.xenoteer.profile-revision="phase-0"

COPY container/packages/runtime.txt /tmp/xenoteer-runtime-packages.txt
RUN rm -f /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/" \
      'Suites: stable stable-updates' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      '' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}/" \
      'Suites: stable-security' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      > /etc/apt/sources.list.d/xenoteer.sources \
    && apt-get update \
    && sed '/^#/d; /^$/d' /tmp/xenoteer-runtime-packages.txt \
      | xargs -r apt-get install -y --no-install-recommends \
    && rm -rf /var/lib/apt/lists/* /tmp/xenoteer-runtime-packages.txt \
    && sed -i 's/^# *en_US.UTF-8 UTF-8/en_US.UTF-8 UTF-8/' /etc/locale.gen \
    && locale-gen \
    && groupadd --gid 1000 xenoteer \
    && useradd --uid 1000 --gid 1000 --home-dir /home/xenoteer --create-home --shell /usr/sbin/nologin xenoteer \
    && install -d -m 0700 -o 1000 -g 1000 /home/xenoteer \
    && install -d -m 0755 /workspace /usr/share/doc/xenoteer /usr/share/xenoteer \
    && rm -f /etc/machine-id \
    && ln -s /run/xenoteer/machine-id /etc/machine-id

COPY --from=s6-overlay /s6-root /
COPY --from=rust-builder /src/target/release/xenoteerd /usr/local/bin/xenoteerd
COPY --from=rust-builder /src/target/release/cargo-components.tsv /usr/share/doc/xenoteer/cargo-components.tsv
COPY --from=rust-builder /src/target/release/cargo-components.spdx.json /usr/share/doc/xenoteer/cargo-components.spdx.json
COPY --from=s6-overlay /tmp/s6-overlay-files.tsv /usr/share/doc/xenoteer/s6-overlay-files.tsv
COPY --from=s6-overlay /tmp/s6-overlay-COPYING /usr/share/doc/s6-overlay/COPYING
COPY container/rootfs/ /
COPY LICENSE /usr/share/doc/xenoteer/LICENSE
COPY NOTICE /usr/share/doc/xenoteer/NOTICE
COPY container/locks/sources.lock /usr/share/doc/xenoteer/sources.lock
COPY container/licenses/image-first-party-paths.tsv /usr/share/xenoteer/image-first-party-paths.tsv
COPY container/licenses/final-image-exceptions.tsv /usr/share/xenoteer/final-image-exceptions.tsv
COPY scripts/licenses/inventory-debian.sh /usr/local/libexec/xenoteer/inventory-debian
COPY scripts/licenses/inventory-image-first-party.sh /usr/local/libexec/xenoteer/inventory-image-first-party
COPY scripts/licenses/inventory-final-image.sh /usr/local/libexec/xenoteer/inventory-final-image
RUN /usr/local/libexec/xenoteer/inventory-debian / /usr/share/doc/xenoteer/package-manifest.tsv \
    && /usr/local/libexec/xenoteer/inventory-image-first-party / /usr/share/doc/xenoteer/first-party-files.tsv \
    && /usr/local/libexec/xenoteer/inventory-final-image / /usr/share/doc/xenoteer/final-files.tsv \
    && chown root:root /usr/local/bin/xenoteerd \
    && chmod 0755 /usr/local/bin/xenoteerd \
    && find /etc/s6-overlay/s6-rc.d /usr/local/libexec/xenoteer -type f \
      \( -name run -o -name finish -o -name up -o -name down -o -name check -o -path '/usr/local/libexec/xenoteer/*' \) \
      -exec chmod 0755 {} +

ENV S6_BEHAVIOUR_IF_STAGE2_FAILS=2 \
    S6_KEEP_ENV=0 \
    S6_SERVICES_READYTIME=5000 \
    S6_KILL_GRACETIME=10000 \
    S6_SERVICES_GRACETIME=15000 \
    DISPLAY=:99 \
    XVFB_SCREEN_WIDTH=1920 \
    XVFB_SCREEN_HEIGHT=1080 \
    XVFB_SCREEN_DEPTH=24 \
    DESKTOP_PROFILE=bare \
    XENOTEER__SERVER__LISTEN=0.0.0.0:8080 \
    XENOTEER__AUTH__TOKEN_FILE=/run/secrets/xenoteer_api_token

EXPOSE 8080
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
  CMD ["/usr/local/libexec/xenoteer/healthcheck"]
USER root
ENTRYPOINT ["/init"]
