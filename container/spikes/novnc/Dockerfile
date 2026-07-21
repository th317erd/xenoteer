# syntax=docker/dockerfile:1.7
# SPDX-License-Identifier: BUSL-1.1

ARG FIXTURE_BUILDER_IMAGE=rust:1.97.1-slim-bookworm@sha256:99e09cb2284e2ddbb73a995deee3e91783fd04d177602ccf6eab326d778ee777
ARG SPIKE_BASE_IMAGE=xenoteer:phase0

FROM ${FIXTURE_BUILDER_IMAGE} AS fixture-builder
WORKDIR /fixture
COPY fixtures/x11/Cargo.toml fixtures/x11/Cargo.lock ./
COPY fixtures/x11/src/ src/
RUN nice -n 10 cargo build --locked --release --jobs 4 \
    --bin x11-event-recorder \
    --bin x11-input-driver

FROM ${SPIKE_BASE_IMAGE} AS novnc-spike
ARG DEBIAN_SNAPSHOT=20260719T000000Z
LABEL org.opencontainers.image.title="Xenoteer noVNC phase-0 spike" \
      org.opencontainers.image.description="Disposable real X0tigervnc/websockify/noVNC conformance image" \
      org.opencontainers.image.licenses="NOASSERTION" \
      com.aeor.xenoteer.spike="phase-0-novnc" \
      com.aeor.xenoteer.distributable="false" \
      com.aeor.xenoteer.debian.snapshot="$DEBIAN_SNAPSHOT" \
      com.aeor.xenoteer.viewer.adapter="X0tigervnc" \
      com.aeor.xenoteer.viewer.adapter.package="tigervnc-scraping-server=1.15.0+dfsg-2.1~deb13u1" \
      com.aeor.xenoteer.viewer.input-policy="server-side-view-only"

USER root
COPY container/spikes/novnc/packages.lock /tmp/novnc-spike/packages.lock
COPY container/spikes/novnc/critical-assets.sha256 /tmp/novnc-spike/critical-assets.sha256
RUN apt-get update \
    && apt-get install --download-only -y --no-install-recommends \
      chromium=150.0.7871.124-1~deb13u1 \
      chromium-sandbox=150.0.7871.124-1~deb13u1 \
      novnc=1:1.6.0-2 \
      tigervnc-scraping-server=1.15.0+dfsg-2.1~deb13u1 \
      websockify=0.12.0+dfsg1-4+b1 \
      xclip=0.13-4 \
      xdotool=1:3.20160805.1-5.1 \
    && printf '%s  %s\n' \
      '834bc56b49d763a18a949a08f41495d0e0a7ddd76a7c73246fd6eddd2d6f9d1d' \
      '/var/cache/apt/archives/chromium_150.0.7871.124-1~deb13u1_amd64.deb' \
      'cae886ad357273dc7ba70c4d7c92f49b00fde98dd9bb56795831b7be8fa13e66' \
      '/var/cache/apt/archives/chromium-sandbox_150.0.7871.124-1~deb13u1_amd64.deb' \
      '7943751137815b9b98c7b424413de78aefa8a1045129ac06c001e9e68e0de98e' \
      '/var/cache/apt/archives/novnc_1%3a1.6.0-2_all.deb' \
      'bc94e6d54b086d8e319228276a7528451113888beb164c1a39d98361c49398b0' \
      '/var/cache/apt/archives/tigervnc-scraping-server_1.15.0+dfsg-2.1~deb13u1_amd64.deb' \
      'e13a821ddaf7ee5cbc16dc930cfa3a83af5ae65ab1c2cc61dcc6c78af0a1fa5f' \
      '/var/cache/apt/archives/websockify_0.12.0+dfsg1-4+b1_amd64.deb' \
      '14203d9a0138996c647d603fcadd79e6765a0a124f606e386215325ac13ece76' \
      '/var/cache/apt/archives/xclip_0.13-4_amd64.deb' \
      '98ac8681533b01020c7288fb2e9de403d586747dab48cfd58e04cb00d0c44fb1' \
      '/var/cache/apt/archives/xdotool_1%3a3.20160805.1-5.1_amd64.deb' \
      | sha256sum --check --strict - \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      chromium=150.0.7871.124-1~deb13u1 \
      chromium-sandbox=150.0.7871.124-1~deb13u1 \
      novnc=1:1.6.0-2 \
      tigervnc-scraping-server=1.15.0+dfsg-2.1~deb13u1 \
      websockify=0.12.0+dfsg1-4+b1 \
      xclip=0.13-4 \
      xdotool=1:3.20160805.1-5.1 \
    && tab="$(printf '\t')" \
    && while IFS="$tab" read -r package version architecture _checksum _license _purpose; do \
      case "$package" in \#*|'') continue ;; esac; \
      actual="$(dpkg-query -W -f='${Version}\t${Architecture}' "$package")"; \
      test "$actual" = "$version${tab}$architecture"; \
    done < /tmp/novnc-spike/packages.lock \
    && grep -v '^#' /tmp/novnc-spike/critical-assets.sha256 \
      | sha256sum --check --strict - \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*.deb

COPY container/spikes/novnc/mandatory.json /usr/share/novnc/mandatory.json
COPY container/spikes/novnc/browser-proof.html /usr/share/novnc/xenoteer-browser-proof.html
COPY --from=fixture-builder /fixture/target/release/x11-event-recorder /usr/local/bin/x11-event-recorder
COPY --from=fixture-builder /fixture/target/release/x11-input-driver /usr/local/bin/x11-input-driver
COPY container/spikes/novnc/rfb_websocket_probe.py /usr/local/libexec/xenoteer/novnc-rfb-probe
COPY container/spikes/novnc/run-spike.sh /usr/local/libexec/xenoteer/run-novnc-spike
COPY container/spikes/novnc/packages.lock /usr/share/doc/xenoteer-novnc-spike/packages.lock
COPY container/spikes/novnc/critical-assets.sha256 /usr/share/doc/xenoteer-novnc-spike/critical-assets.sha256
COPY container/spikes/novnc/README.md /usr/share/doc/xenoteer-novnc-spike/README.md
RUN chmod 0755 \
      /usr/local/bin/x11-event-recorder \
      /usr/local/bin/x11-input-driver \
      /usr/local/libexec/xenoteer/novnc-rfb-probe \
      /usr/local/libexec/xenoteer/run-novnc-spike \
    && install -d -m 0755 /usr/share/doc/xenoteer-novnc-spike \
    && { \
      printf 'binary_package\tbinary_version\tarchitecture\tsource_package\tsource_version\n'; \
      dpkg-query -W -f='${Package}\t${Version}\t${Architecture}\t${source:Package}\t${source:Version}\n' \
        | LC_ALL=C sort; \
    } > /usr/share/doc/xenoteer-novnc-spike/dpkg-manifest.tsv \
    && for package in chromium chromium-sandbox novnc tigervnc-scraping-server websockify xclip xdotool; do \
      sed -n 's/^License: //p' "/usr/share/doc/$package/copyright" \
        | LC_ALL=C sort -u \
        | while IFS= read -r license; do printf '%s\t%s\n' "$package" "$license"; done; \
    done > /usr/share/doc/xenoteer-novnc-spike/direct-license-stanzas.tsv \
    && find /usr/share/novnc -type f -print0 \
      | LC_ALL=C sort -z \
      | xargs -0 sha256sum \
      > /usr/share/doc/xenoteer-novnc-spike/installed-novnc-assets.sha256 \
    && for package in chromium chromium-sandbox novnc tigervnc-scraping-server websockify xclip xdotool; do \
      test -r "/usr/share/doc/$package/copyright"; \
    done

ENTRYPOINT ["/usr/local/libexec/xenoteer/run-novnc-spike"]
