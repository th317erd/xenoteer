ARG XENOTEER_BASE_IMAGE=xenoteer:dev
FROM ${XENOTEER_BASE_IMAGE}

ARG XENOTEER_FIXTURE_BASE_IMAGE_ID
ARG ELECTRON_VERSION
ARG ELECTRON_LINUX_X64_URL
ARG ELECTRON_LINUX_X64_SHA256
LABEL com.aeor.xenoteer.distribution-scope="test-only-non-distributable" \
      com.aeor.xenoteer.fixture="phase-2-desktop-apps" \
      com.aeor.xenoteer.fixture.debian-snapshot="20260719T000000Z" \
      com.aeor.xenoteer.fixture.base-image-id="${XENOTEER_FIXTURE_BASE_IMAGE_ID}" \
      com.aeor.xenoteer.fixture.electron-version="${ELECTRON_VERSION}" \
      com.aeor.xenoteer.fixture.electron-linux-x64-sha256="${ELECTRON_LINUX_X64_SHA256}"

USER root
COPY container/fixtures/desktop-apps/packages.lock /tmp/xenoteer-desktop-apps-packages.lock
COPY container/fixtures/desktop-apps/artifacts.lock /usr/share/doc/xenoteer/desktop-app-artifacts.lock
RUN nice -n 15 apt-get update \
    && sed '/^#/d; /^$/d' /tmp/xenoteer-desktop-apps-packages.lock \
      | xargs -r env DEBIAN_FRONTEND=noninteractive \
        nice -n 15 apt-get install -y --no-install-recommends \
    && rm -rf /var/lib/apt/lists/* /tmp/xenoteer-desktop-apps-packages.lock \
    && test -n "$XENOTEER_FIXTURE_BASE_IMAGE_ID" \
    && test -n "$ELECTRON_VERSION" \
    && test -n "$ELECTRON_LINUX_X64_URL" \
    && test -n "$ELECTRON_LINUX_X64_SHA256" \
    && curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
      --retry 5 --output /tmp/electron-linux-x64.zip "$ELECTRON_LINUX_X64_URL" \
    && printf '%s  %s\n' "$ELECTRON_LINUX_X64_SHA256" /tmp/electron-linux-x64.zip \
      | sha256sum --check --strict - \
    && install -d -m 0755 /opt/xenoteer-electron \
    && nice -n 15 python3 -m zipfile -e \
      /tmp/electron-linux-x64.zip /opt/xenoteer-electron \
    && test "$(cat /opt/xenoteer-electron/version)" = "$ELECTRON_VERSION" \
    && test -s /opt/xenoteer-electron/LICENSE \
    && test -s /opt/xenoteer-electron/LICENSES.chromium.html \
    && chown -R root:root /opt/xenoteer-electron \
    && chmod 0755 /opt/xenoteer-electron/electron \
    && chmod 4755 /opt/xenoteer-electron/chrome-sandbox \
    && rm -f /tmp/electron-linux-x64.zip
COPY --chown=root:root container/rootfs/usr/share/xenoteer/fixtures/desktop-apps/ \
  /usr/share/xenoteer/fixtures/desktop-apps/
