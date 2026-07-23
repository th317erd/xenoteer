# syntax=docker/dockerfile:1.20.0@sha256:26147acbda4f14c5add9946e2fd2ed543fc402884fd75146bd342a7f6271dc1d
ARG DEBIAN_BASE_IMAGE=debian:13.6-slim@sha256:328d16499860ae6cb9b345e2e4cebca08c2a36e4f7278482c7bd1f39d71e5bfd
ARG RUST_BUILDER_IMAGE=rust:1.97.1-slim-bookworm@sha256:99e09cb2284e2ddbb73a995deee3e91783fd04d177602ccf6eab326d778ee777

FROM ${RUST_BUILDER_IMAGE} AS rust-builder
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ARG DEBIAN_SNAPSHOT=20260719T000000Z
ARG RUST_BUILDER_DEBIAN_SUITE=bookworm
ARG RUST_BUILDER_DEBIAN_INRELEASE_SHA256=77737fa4b34f2693e982cc9ee35736816c35a7778fc2d326cc1bbf5b301fe1aa
ARG RUST_BUILDER_DEBIAN_UPDATES_INRELEASE_SHA256=ff485a327a57d1cc35a5d12548171fd48f9525dfa7bd4e97570fcf738cf1112a
ARG RUST_BUILDER_DEBIAN_SECURITY_INRELEASE_SHA256=f1cfa9017f64f876dd5d443dc8cfa0c831958f0123fb137ecd3c26cce27109a4
WORKDIR /src
COPY container/packages/builder.txt /tmp/xenoteer-builder-packages.txt
# hadolint ignore=DL3008
RUN --mount=type=bind,source=scripts/container/verify-apt-metadata.sh,target=/usr/local/bin/verify-apt-metadata,ro \
    rm -f /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources \
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
    && /usr/local/bin/verify-apt-metadata \
      "$DEBIAN_SNAPSHOT" "$RUST_BUILDER_DEBIAN_SUITE" \
      "$RUST_BUILDER_DEBIAN_INRELEASE_SHA256" \
      "$RUST_BUILDER_DEBIAN_UPDATES_INRELEASE_SHA256" \
      "$RUST_BUILDER_DEBIAN_SECURITY_INRELEASE_SHA256" \
    && sed '/^#/d; /^$/d' /tmp/xenoteer-builder-packages.txt \
      | xargs -r apt-get install -y --no-install-recommends \
    && rm -rf /var/lib/apt/lists/* /tmp/xenoteer-builder-packages.txt
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo/ .cargo/
COPY crates/ crates/
COPY scripts/licenses/generate-cargo-manifest.sh /usr/local/bin/generate-cargo-manifest
RUN chmod 0755 /usr/local/bin/generate-cargo-manifest \
    && nice -n 10 cargo build --locked --release \
      --bin xenoteerd --bin xenoteer-processd --jobs 2 \
    && /usr/local/bin/generate-cargo-manifest \
      /src \
      /src/target/release/xenoteerd \
      /src/target/release/cargo-components.tsv \
      /src/target/release/cargo-components.spdx.json

FROM ${DEBIAN_BASE_IMAGE} AS s6-overlay
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ARG DEBIAN_SNAPSHOT=20260719T000000Z
ARG DEBIAN_SUITE=trixie
ARG DEBIAN_INRELEASE_SHA256=98b25b5cd185c59d34aa6e4c3e9b5b8f01bbe9d104fe2dcfbcd30dc0a14a59ed
ARG DEBIAN_UPDATES_INRELEASE_SHA256=d761d119a8504b6d1c80fa1d7b851583875eef0702177bd79ef644c58416dfb6
ARG DEBIAN_SECURITY_INRELEASE_SHA256=4819a1f38724b97053cc49f3e567b8ce240bb0e29eed735aa40312f9b6c9daf0
ARG S6_OVERLAY_VERSION=3.2.2.0
ARG S6_OVERLAY_ARCH=x86_64
ARG S6_OVERLAY_NOARCH_SHA256=85848f6baab49fb7832a5557644c73c066899ed458dd1601035cf18e7c759f26
ARG S6_OVERLAY_ARCH_SHA256=5a09e2f1878dc5f7f0211dd7bafed3eee1afe4f813e872fff2ab1957f266c7c0
ARG S6_OVERLAY_COPYING_SHA256=7184c7d1dae02fc4a23e0d2cda2c8a107ba08fbc0158bc25f4d0f404941780db
COPY scripts/licenses/generate-s6-manifest.sh /usr/local/bin/generate-s6-manifest
# hadolint ignore=DL3008
RUN --mount=type=bind,source=scripts/container/verify-apt-metadata.sh,target=/usr/local/bin/verify-apt-metadata,ro \
    rm -f /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/" \
      "Suites: ${DEBIAN_SUITE} ${DEBIAN_SUITE}-updates" \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      '' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}/" \
      "Suites: ${DEBIAN_SUITE}-security" \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      > /etc/apt/sources.list.d/xenoteer.sources \
    && apt-get update \
    && /usr/local/bin/verify-apt-metadata \
      "$DEBIAN_SNAPSHOT" "$DEBIAN_SUITE" \
      "$DEBIAN_INRELEASE_SHA256" "$DEBIAN_UPDATES_INRELEASE_SHA256" \
      "$DEBIAN_SECURITY_INRELEASE_SHA256" \
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

FROM ${DEBIAN_BASE_IMAGE} AS novnc-assets
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ARG DEBIAN_SNAPSHOT=20260719T000000Z
ARG DEBIAN_SUITE=trixie
ARG DEBIAN_INRELEASE_SHA256=98b25b5cd185c59d34aa6e4c3e9b5b8f01bbe9d104fe2dcfbcd30dc0a14a59ed
ARG DEBIAN_UPDATES_INRELEASE_SHA256=d761d119a8504b6d1c80fa1d7b851583875eef0702177bd79ef644c58416dfb6
ARG DEBIAN_SECURITY_INRELEASE_SHA256=4819a1f38724b97053cc49f3e567b8ce240bb0e29eed735aa40312f9b6c9daf0
ARG NOVNC_VERSION=1:1.6.0-2
ARG NOVNC_DEB_SHA256=7943751137815b9b98c7b424413de78aefa8a1045129ac06c001e9e68e0de98e
COPY container/locks/novnc-critical-assets.sha256 /tmp/novnc-critical-assets.sha256
COPY scripts/licenses/generate-novnc-manifest.sh /usr/local/bin/generate-novnc-manifest
# hadolint ignore=DL3003
RUN --mount=type=bind,source=scripts/container/verify-apt-metadata.sh,target=/usr/local/bin/verify-apt-metadata,ro \
    rm -f /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/" \
      "Suites: ${DEBIAN_SUITE} ${DEBIAN_SUITE}-updates" \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      '' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}/" \
      "Suites: ${DEBIAN_SUITE}-security" \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      > /etc/apt/sources.list.d/xenoteer.sources \
    && apt-get update \
    && /usr/local/bin/verify-apt-metadata \
      "$DEBIAN_SNAPSHOT" "$DEBIAN_SUITE" \
      "$DEBIAN_INRELEASE_SHA256" "$DEBIAN_UPDATES_INRELEASE_SHA256" \
      "$DEBIAN_SECURITY_INRELEASE_SHA256" \
    && install -d -m 0755 /tmp/novnc-download /novnc-root/usr/share/doc/novnc \
    && cd /tmp/novnc-download \
    && apt-get download "novnc=${NOVNC_VERSION}" \
    && novnc_deb=$(find . -maxdepth 1 -type f -name 'novnc_*.deb' -print -quit) \
    && test -n "$novnc_deb" \
    && printf '%s  %s\n' "$NOVNC_DEB_SHA256" "$novnc_deb" \
      | sha256sum --check --strict - \
    && test "$(dpkg-deb -f "$novnc_deb" Package)" = novnc \
    && test "$(dpkg-deb -f "$novnc_deb" Version)" = "$NOVNC_VERSION" \
    && test "$(dpkg-deb -f "$novnc_deb" Architecture)" = all \
    && install -d -m 0755 /tmp/novnc-unpack \
    && dpkg-deb -x "$novnc_deb" /tmp/novnc-unpack \
    && cp -a /tmp/novnc-unpack/usr/share/novnc /novnc-root/usr/share/ \
    && cp -a /tmp/novnc-unpack/usr/share/doc/novnc/copyright \
      /novnc-root/usr/share/doc/novnc/copyright \
    && rm -f /novnc-root/usr/share/novnc/mandatory.json \
    && chmod 0755 /usr/local/bin/generate-novnc-manifest \
    && /usr/local/bin/generate-novnc-manifest \
      /novnc-root /tmp/novnc-files.tsv /tmp/novnc-critical-assets.sha256 \
    && rm -rf /var/lib/apt/lists/* /tmp/novnc-download /tmp/novnc-unpack

FROM ${DEBIAN_BASE_IMAGE} AS runtime
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
ARG DEBIAN_BASE_DIGEST=sha256:328d16499860ae6cb9b345e2e4cebca08c2a36e4f7278482c7bd1f39d71e5bfd
ARG DEBIAN_SNAPSHOT=20260719T000000Z
ARG DEBIAN_SUITE=trixie
ARG DEBIAN_INRELEASE_SHA256=98b25b5cd185c59d34aa6e4c3e9b5b8f01bbe9d104fe2dcfbcd30dc0a14a59ed
ARG DEBIAN_UPDATES_INRELEASE_SHA256=d761d119a8504b6d1c80fa1d7b851583875eef0702177bd79ef644c58416dfb6
ARG DEBIAN_SECURITY_INRELEASE_SHA256=4819a1f38724b97053cc49f3e567b8ce240bb0e29eed735aa40312f9b6c9daf0
ARG PYTHON3_WEBSOCKIFY_VERSION=0.12.0+dfsg1-4+b1
ARG PYTHON3_WEBSOCKIFY_DEB_SHA256=2cc697df15126b6561a557323c17d414df5693580f4618644e76cf9d91baf53f
ARG TIGERVNC_SCRAPING_SERVER_VERSION=1.15.0+dfsg-2.1~deb13u1
ARG TIGERVNC_SCRAPING_SERVER_DEB_SHA256=bc94e6d54b086d8e319228276a7528451113888beb164c1a39d98361c49398b0
COPY container/packages/runtime.txt /tmp/xenoteer-runtime-packages.txt
COPY container/packages/desktop.txt /tmp/xenoteer-desktop-packages.txt
COPY container/packages/viewer.txt /tmp/xenoteer-viewer-packages.txt
COPY scripts/licenses/inventory-debian.sh /usr/local/libexec/xenoteer/inventory-debian
COPY scripts/licenses/generate-debian-installed-manifest.sh /usr/local/libexec/xenoteer/generate-debian-installed-manifest
# hadolint ignore=DL3003,DL3008
RUN --mount=type=bind,source=scripts/container/verify-apt-metadata.sh,target=/usr/local/bin/verify-apt-metadata,ro \
    rm -f /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/" \
      "Suites: ${DEBIAN_SUITE} ${DEBIAN_SUITE}-updates" \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      '' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}/" \
      "Suites: ${DEBIAN_SUITE}-security" \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      'Check-Valid-Until: no' \
      > /etc/apt/sources.list.d/xenoteer.sources \
    && apt-get update \
    && /usr/local/bin/verify-apt-metadata \
      "$DEBIAN_SNAPSHOT" "$DEBIAN_SUITE" \
      "$DEBIAN_INRELEASE_SHA256" "$DEBIAN_UPDATES_INRELEASE_SHA256" \
      "$DEBIAN_SECURITY_INRELEASE_SHA256" \
    && sed '/^#/d; /^$/d' \
      /tmp/xenoteer-runtime-packages.txt /tmp/xenoteer-desktop-packages.txt \
      | xargs -r env DEBIAN_FRONTEND=noninteractive \
        apt-get install -y --no-install-recommends \
    && install -d -m 0755 /tmp/viewer-debs \
    && cd /tmp/viewer-debs \
    && apt-get download \
      "python3-websockify=${PYTHON3_WEBSOCKIFY_VERSION}" \
      "tigervnc-scraping-server=${TIGERVNC_SCRAPING_SERVER_VERSION}" \
    && websockify_deb=$(find . -maxdepth 1 -type f -name 'python3-websockify_*.deb' -print -quit) \
    && tigervnc_deb=$(find . -maxdepth 1 -type f -name 'tigervnc-scraping-server_*.deb' -print -quit) \
    && test -n "$websockify_deb" && test -n "$tigervnc_deb" \
    && printf '%s  %s\n%s  %s\n' \
      "$PYTHON3_WEBSOCKIFY_DEB_SHA256" "$websockify_deb" \
      "$TIGERVNC_SCRAPING_SERVER_DEB_SHA256" "$tigervnc_deb" \
      | sha256sum --check --strict - \
    && test "$(dpkg-deb -f "$websockify_deb" Package)" = python3-websockify \
    && test "$(dpkg-deb -f "$websockify_deb" Version)" = "$PYTHON3_WEBSOCKIFY_VERSION" \
    && test "$(dpkg-deb -f "$websockify_deb" Architecture)" = amd64 \
    && test "$(dpkg-deb -f "$tigervnc_deb" Package)" = tigervnc-scraping-server \
    && test "$(dpkg-deb -f "$tigervnc_deb" Version)" = "$TIGERVNC_SCRAPING_SERVER_VERSION" \
    && test "$(dpkg-deb -f "$tigervnc_deb" Architecture)" = amd64 \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      "$websockify_deb" "$tigervnc_deb" \
    && cd / \
    && sed -i 's/^# *en_US.UTF-8 UTF-8/en_US.UTF-8 UTF-8/' /etc/locale.gen \
    && locale-gen \
    && groupadd --gid 1000 xenoteer \
    && groupadd --gid 1001 xenoteerd \
    && useradd --uid 1000 --gid 1000 --home-dir /home/xenoteer --create-home --shell /usr/sbin/nologin xenoteer \
    && useradd --uid 1001 --gid 1001 --groups 1000 --home-dir /nonexistent --no-create-home --shell /usr/sbin/nologin xenoteerd \
    && install -d -m 0700 -o 1000 -g 1000 /home/xenoteer \
    && install -d -m 0755 -o 1000 -g 1000 /workspace \
    && install -d -m 0755 /usr/share/doc/xenoteer /usr/share/xenoteer \
    && rm -f /etc/machine-id \
    && ln -s /run/xenoteer/machine-id /etc/machine-id \
    && cat /tmp/xenoteer-runtime-packages.txt /tmp/xenoteer-desktop-packages.txt \
      /tmp/xenoteer-viewer-packages.txt > /tmp/xenoteer-direct-packages.txt \
    && chmod 0755 /usr/local/libexec/xenoteer/inventory-debian \
      /usr/local/libexec/xenoteer/generate-debian-installed-manifest \
    && /usr/local/libexec/xenoteer/inventory-debian \
      / /usr/share/doc/xenoteer/package-manifest.tsv /tmp/xenoteer-direct-packages.txt \
    && for forbidden in dbus-x11 novnc websockify nodejs net-tools thunar tumbler \
      lightdm gdm3 sddm xfce4-power-manager xfce4-notifyd; do \
      ! dpkg-query -W -f='${db:Status-Status}' "$forbidden" 2>/dev/null \
        | grep -Fqx installed; \
    done \
    && command -v dbus-daemon >/dev/null \
    && command -v gdbus >/dev/null \
    && test -x /usr/libexec/at-spi-bus-launcher \
    && command -v xfce4-session >/dev/null \
    && command -v X0tigervnc >/dev/null \
    && command -v websockify >/dev/null \
    && ! command -v dbus-launch >/dev/null \
    && fc-cache --force \
    && test "$(fc-match --format='%{family[0]}' 'DejaVu Sans')" = 'DejaVu Sans' \
    && test "$(fc-match --format='%{family[0]}' 'Liberation Sans')" = 'Liberation Sans' \
    && test "$(fc-match --format='%{family[0]}' 'Noto Sans')" = 'Noto Sans' \
    && test "$(fc-match --format='%{family[0]}' 'Noto Sans CJK JP')" = 'Noto Sans CJK JP' \
    && test "$(fc-match --format='%{family[0]}' 'Noto Color Emoji')" = 'Noto Color Emoji' \
    && test "$(fc-match --format='%{family[0]}' 'Noto Sans Mono')" = 'Noto Sans Mono' \
    # AT-SPI is an explicit supervised critical service. Leaving the package's
    # session-bus activation entry installed permits an unsupervised launcher
    # to race a supervised restart and orphan a second accessibility bus.
    && rm -f /usr/share/dbus-1/services/org.a11y.Bus.service \
    && nice -n 15 /usr/local/libexec/xenoteer/generate-debian-installed-manifest \
      / /usr/share/doc/xenoteer/debian-installed-files.tsv \
      /usr/share/doc/xenoteer/package-manifest.tsv \
    && rm -rf /var/lib/apt/lists/* /tmp/viewer-debs \
      /tmp/xenoteer-runtime-packages.txt /tmp/xenoteer-desktop-packages.txt \
      /tmp/xenoteer-viewer-packages.txt /tmp/xenoteer-direct-packages.txt

COPY --from=s6-overlay /s6-root /
COPY --from=rust-builder /src/target/release/xenoteerd /usr/local/bin/xenoteerd
COPY --from=rust-builder /src/target/release/xenoteer-processd /usr/local/bin/xenoteer-processd
COPY --from=rust-builder /src/target/release/cargo-components.tsv /usr/share/doc/xenoteer/cargo-components.tsv
COPY --from=rust-builder /src/target/release/cargo-components.spdx.json /usr/share/doc/xenoteer/cargo-components.spdx.json
COPY --from=s6-overlay /tmp/s6-overlay-files.tsv /usr/share/doc/xenoteer/s6-overlay-files.tsv
COPY --from=s6-overlay /tmp/s6-overlay-COPYING /usr/share/doc/s6-overlay/COPYING
COPY --from=novnc-assets /novnc-root/usr/share/novnc/ /usr/share/novnc/
COPY --from=novnc-assets /novnc-root/usr/share/doc/novnc/copyright /usr/share/doc/novnc/copyright
COPY --from=novnc-assets /tmp/novnc-files.tsv /usr/share/doc/xenoteer/novnc-files.tsv
COPY container/novnc/mandatory.json /usr/share/novnc/mandatory.json
COPY container/rootfs/ /
COPY LICENSE /usr/share/doc/xenoteer/LICENSE
COPY NOTICE /usr/share/doc/xenoteer/NOTICE
COPY container/locks/release.lock /usr/share/doc/xenoteer/release.lock
COPY container/locks/sources.lock /usr/share/doc/xenoteer/sources.lock
COPY container/licenses/image-first-party-paths.tsv /usr/share/xenoteer/image-first-party-paths.tsv
COPY container/licenses/final-image-exceptions.tsv /usr/share/xenoteer/final-image-exceptions.tsv
COPY scripts/licenses/inventory-image-first-party.sh /usr/local/libexec/xenoteer/inventory-image-first-party
COPY scripts/licenses/inventory-final-image.sh /usr/local/libexec/xenoteer/inventory-final-image
RUN chown root:root /usr/local/bin/xenoteerd /usr/local/bin/xenoteer-processd \
    && chmod 0755 /usr/local/bin/xenoteerd /usr/local/bin/xenoteer-processd \
    && find /etc/s6-overlay/s6-rc.d /usr/local/libexec/xenoteer -type f \
      \( -name run -o -name finish -o -name up -o -name down -o -name check -o -path '/usr/local/libexec/xenoteer/*' \) \
      -exec chmod 0755 {} + \
    && /usr/local/libexec/xenoteer/inventory-image-first-party / /usr/share/doc/xenoteer/first-party-files.tsv \
    && nice -n 15 /usr/local/libexec/xenoteer/inventory-final-image / /usr/share/doc/xenoteer/final-files.tsv

# Source-dependent metadata belongs after all rootfs assembly so a revision-only
# rebuild reuses the expensive verified package and inventory layers exactly.
ARG XENOTEER_VERSION=0.1.0-dev
ARG XENOTEER_REVISION=unknown
ARG XENOTEER_CREATED=1970-01-01T00:00:00Z
ARG XENOTEER_SOURCE_DIRTY=unknown
ARG XENOTEER_SOURCE_TREE_SHA256=unknown
ARG XENOTEER_DEPENDENCY_LOCK_SHA256=unknown
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
      com.aeor.xenoteer.source.dirty="$XENOTEER_SOURCE_DIRTY" \
      com.aeor.xenoteer.source-tree.sha256="$XENOTEER_SOURCE_TREE_SHA256" \
      com.aeor.xenoteer.dependency-lock.sha256="$XENOTEER_DEPENDENCY_LOCK_SHA256" \
      com.aeor.xenoteer.protocol="v1" \
      com.aeor.xenoteer.profile-revision="phase-2" \
      com.aeor.xenoteer.viewer.adapter="X0tigervnc" \
      com.aeor.xenoteer.viewer.input-policy="server-side-view-only" \
      com.aeor.xenoteer.novnc.version="1:1.6.0-2"

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
    XENOTEER__SERVER__LISTEN=0.0.0.0:8080

EXPOSE 8080
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=10s --timeout=3s --start-period=90s --retries=3 \
  CMD ["/usr/local/libexec/xenoteer/healthcheck"]
# hadolint ignore=DL3002
USER root
ENTRYPOINT ["/init"]
