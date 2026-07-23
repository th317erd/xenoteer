#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Visible QtWebEngine/X11 fixture with a writable ephemeral profile."""

import json
import os
import sys
from pathlib import Path

from PyQt6.QtCore import QTimer, QUrl, QUrlQuery
from PyQt6.QtWebEngineCore import QWebEngineProfile
from PyQt6.QtWebEngineWidgets import QWebEngineView
from PyQt6.QtWidgets import QApplication


def main() -> int:
    if os.environ.get("QTWEBENGINE_DISABLE_SANDBOX"):
        raise SystemExit("QTWEBENGINE_DISABLE_SANDBOX must be unset")
    if "--no-sandbox" in os.environ.get("QTWEBENGINE_CHROMIUM_FLAGS", "").split():
        raise SystemExit("QtWebEngine sandbox disabling is forbidden")

    xdg_data = Path(os.environ["XDG_DATA_HOME"])
    xdg_cache = Path(os.environ["XDG_CACHE_HOME"])
    data_path = xdg_data / "xenoteer/browser-profiles/qtwebengine"
    cache_path = xdg_cache / "xenoteer/qtwebengine"
    data_path.mkdir(parents=True, exist_ok=True)
    cache_path.mkdir(parents=True, exist_ok=True)

    app = QApplication(sys.argv)
    app.setApplicationName("Xenoteer QtWebEngine Fixture")
    profile = QWebEngineProfile.defaultProfile()
    profile.setPersistentStoragePath(str(data_path))
    profile.setCachePath(str(cache_path))
    profile.setHttpAcceptLanguage("en-US,en")
    profile.setPersistentCookiesPolicy(QWebEngineProfile.PersistentCookiesPolicy.NoPersistentCookies)

    view = QWebEngineView()
    view.setWindowTitle("Xenoteer QtWebEngine Browser Fixture")
    view.setAccessibleName("Xenoteer QtWebEngine Main Window")
    view.resize(800, 600)
    state = {"loaded": False}

    def fail(message: str) -> None:
        print(message, file=sys.stderr, flush=True)
        app.exit(1)

    def capture(value: object) -> None:
        expected = {
            "title": "Xenoteer QtWebEngine Browser Fixture",
            "browser": "qtwebengine",
            "marker": "phase2-rendered",
            "browserMarker": "QtWebEngine fixture marker content",
            "status": "ready",
        }
        if value != expected:
            fail(f"unexpected QtWebEngine DOM state: {value!r}")
            return
        state["loaded"] = True
        print(json.dumps({"type": "qtwebengine_fixture_ready", "dom": value}, sort_keys=True), flush=True)

    def loaded(ok: bool) -> None:
        if not ok:
            fail("QtWebEngine loadFinished reported failure")
            return
        script = """({
          title: document.title,
          browser: document.body.dataset.browser,
          marker: document.body.dataset.xenoteerMarker,
          browserMarker: document.getElementById('browser-marker').textContent,
          status: document.getElementById('status').textContent
        })"""
        QTimer.singleShot(250, lambda: view.page().runJavaScript(script, capture))

    view.loadFinished.connect(loaded)
    url = QUrl.fromLocalFile("/usr/share/xenoteer/fixtures/desktop-apps/fixture.html")
    query = QUrlQuery()
    query.addQueryItem("browser", "qtwebengine")
    url.setQuery(query)
    view.load(url)
    view.show()
    QTimer.singleShot(30000, lambda: fail("QtWebEngine fixture timed out") if not state["loaded"] else None)
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
