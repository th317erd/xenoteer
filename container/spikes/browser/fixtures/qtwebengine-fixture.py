#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Visible QtWebEngine/X11 render fixture that remains alive for process audit."""

import json
import os
import sys

from PyQt6.QtCore import QTimer, QUrl
from PyQt6.QtWidgets import QApplication
from PyQt6.QtWebEngineWidgets import QWebEngineView


if os.environ.get("QTWEBENGINE_DISABLE_SANDBOX"):
    raise SystemExit("QTWEBENGINE_DISABLE_SANDBOX must be unset")
if "--no-sandbox" in os.environ.get("QTWEBENGINE_CHROMIUM_FLAGS", "").split():
    raise SystemExit("QtWebEngine --no-sandbox is forbidden")

app = QApplication(sys.argv)
view = QWebEngineView()
view.resize(640, 480)
view.show()
state = {"loaded": False}


def fail(message: str) -> None:
    print(message, file=sys.stderr, flush=True)
    app.exit(1)


def capture(value: object) -> None:
    expected = {
        "title": "Xenoteer Phase 0 Browser Fixture",
        "marker": "phase0-rendered",
        "background": "rgb(20, 164, 77)",
    }
    if value != expected:
        fail(f"unexpected QtWebEngine DOM state: {value!r}")
        return
    state["loaded"] = True
    print(json.dumps({"type": "qtwebengine_spike_ready", "dom": value}, sort_keys=True), flush=True)


def loaded(ok: bool) -> None:
    if not ok:
        fail("QtWebEngine loadFinished reported failure")
        return
    script = """({
      title: document.title,
      marker: document.body.dataset.xenoteerMarker,
      background: getComputedStyle(document.body).backgroundColor
    })"""
    QTimer.singleShot(1000, lambda: view.page().runJavaScript(script, capture))


view.loadFinished.connect(loaded)
view.load(QUrl.fromLocalFile("/opt/xenoteer-spikes/browser/phase0-browser.html"))
QTimer.singleShot(30000, lambda: fail("QtWebEngine fixture timed out") if not state["loaded"] else None)
QTimer.singleShot(60000, app.quit)
raise SystemExit(app.exec())
