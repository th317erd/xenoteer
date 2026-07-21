ARG XENOTEER_RUNTIME_IMAGE=xenoteer:phase0
FROM ${XENOTEER_RUNTIME_IMAGE}

USER root
COPY container/spikes/browser/packages.txt /tmp/xenoteer-browser-spike-packages.txt
RUN apt-get update \
    && sed '/^#/d; /^$/d' /tmp/xenoteer-browser-spike-packages.txt \
      | xargs -r apt-get install -y --no-install-recommends \
    && rm -rf /var/lib/apt/lists/* /tmp/xenoteer-browser-spike-packages.txt

COPY container/spikes/browser/fixtures/ /opt/xenoteer-spikes/browser/
RUN chmod 0755 \
      /opt/xenoteer-spikes/browser/chromium-devtools-probe.py \
      /opt/xenoteer-spikes/browser/qtwebengine-fixture.py \
      /opt/xenoteer-spikes/browser/run-chromium-spike \
      /opt/xenoteer-spikes/browser/run-qtwebengine-spike \
    && /usr/local/libexec/xenoteer/inventory-debian \
      / /usr/share/doc/xenoteer/browser-spike-package-manifest.tsv

LABEL com.aeor.xenoteer.test-stage="browser-sandbox-spike" \
      com.aeor.xenoteer.distributable="false" \
      com.aeor.xenoteer.chromium.version="150.0.7871.124-1~deb13u1" \
      com.aeor.xenoteer.pyqt6-webengine.version="6.9.0-1"
