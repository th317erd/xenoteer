#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Deterministic Qt6 Widgets/X11 fixture with stable accessibility names."""

import argparse
import json
import os
from pathlib import Path

from PyQt6.QtCore import Qt
from PyQt6.QtGui import QAction
from PyQt6.QtWidgets import (
    QApplication,
    QCheckBox,
    QComboBox,
    QDialog,
    QGridLayout,
    QLabel,
    QLineEdit,
    QListWidget,
    QMenuBar,
    QPushButton,
    QRadioButton,
    QSlider,
    QSpinBox,
    QTabWidget,
    QTextEdit,
    QToolBar,
    QVBoxLayout,
    QWidget,
)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--ready-file", type=Path)
    args = parser.parse_args()

    if os.environ.get("QT_LINUX_ACCESSIBILITY_ALWAYS_ON") != "1":
        raise SystemExit("QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 is required")

    app = QApplication(["xenoteer-qt6-fixture"])
    app.setApplicationName("Xenoteer Qt6 Fixture")
    app.setApplicationDisplayName("Xenoteer Qt6 Fixture")

    window = QWidget()
    window.setWindowTitle("Xenoteer Qt6 Fixture — Main")
    window.setObjectName("xenoteerQt6Main")
    window.setAccessibleName("Xenoteer Qt6 Main Window")
    window.setFixedSize(900, 700)
    grid = QGridLayout(window)

    menubar = QMenuBar()
    fixture_menu = menubar.addMenu("Fixture menu")
    fixture_menu.setAccessibleName("Stable Menu")
    menu_action = QAction("Nested menu action", window)
    menu_action.setObjectName("nestedMenuAction")
    fixture_menu.addAction(menu_action)
    toolbar = QToolBar("Fixture toolbar")
    toolbar.setAccessibleName("Stable Toolbar")
    toolbar_action = QAction("Toolbar action", window)
    toolbar.addAction(toolbar_action)
    grid.addWidget(menubar, 0, 0, 1, 2)
    grid.addWidget(toolbar, 1, 0, 1, 2)

    heading = QLabel("Xenoteer deterministic Qt6 fixture")
    heading.setAccessibleName("Fixture Heading")
    entry = QLineEdit("editable fixture text")
    entry.setAccessibleName("Stable Entry")
    protected = QLineEdit("fixture-secret")
    protected.setEchoMode(QLineEdit.EchoMode.Password)
    protected.setAccessibleName("Protected Entry")
    checkbox = QCheckBox("Stable checkbox")
    checkbox.setChecked(True)
    checkbox.setAccessibleName("Stable Checkbox")
    button = QPushButton("Stable button")
    button.setAccessibleName("Stable Button")
    activation_count = [0]
    activation_status = QLabel("Activation count: 0")
    activation_status.setAccessibleName("Activation Count 0")

    def record_activation() -> None:
        activation_count[0] += 1
        activation_status.setText(f"Activation count: {activation_count[0]}")
        activation_status.setAccessibleName(f"Activation Count {activation_count[0]}")

    button.clicked.connect(record_activation)
    double_click = QPushButton("Double-click target")
    double_click.setAccessibleName("Double-click Target")
    disabled = QPushButton("Disabled button")
    disabled.setEnabled(False)
    disabled.setAccessibleName("Disabled Button")
    combo = QComboBox()
    combo.addItems(["Alpha", "Beta", "Gamma"])
    combo.setCurrentIndex(1)
    combo.setAccessibleName("Stable Choice")
    radio_alpha = QRadioButton("Alpha radio")
    radio_beta = QRadioButton("Beta radio")
    radio_alpha.setChecked(True)
    radio_alpha.setAccessibleName("Stable Radio Alpha")
    radio_beta.setAccessibleName("Stable Radio Beta")
    slider = QSlider(Qt.Orientation.Horizontal)
    slider.setValue(40)
    slider.setAccessibleName("Stable Slider")
    spinner = QSpinBox()
    spinner.setValue(7)
    spinner.setAccessibleName("Stable Spin")
    text_area = QTextEdit("editable multiline fixture text")
    text_area.setAccessibleName("Stable Text Area")
    tabs = QTabWidget()
    tabs.setAccessibleName("Stable Tabs")
    tabs.addTab(QLabel("First tab content"), "First tab")
    tabs.addTab(QLabel("Second tab content"), "Second tab")
    virtual_list = QListWidget()
    virtual_list.addItems([f"Virtual row {index:02d}" for index in range(64)])
    virtual_list.setAccessibleName("Stable Virtual List")
    custom_area = QWidget()
    custom_area.setFixedSize(240, 120)
    custom_area.setToolTip("Stable drawing tooltip")
    custom_area.setAccessibleName("Stable Custom Area")
    fonts = QLabel("Latin — العربية — 中文 — e\u0301 — 😀")
    fonts.setLayoutDirection(Qt.LayoutDirection.LeftToRight)
    fonts.setAccessibleName("Font Coverage")
    controls = (
        heading, entry, protected, checkbox, radio_alpha, radio_beta, slider,
        spinner, combo, button, activation_status, double_click, disabled, text_area,
        fonts,
    )
    for row, widget in enumerate(controls, start=2):
        grid.addWidget(widget, row, 0)
    grid.addWidget(tabs, 2, 1, 3, 1)
    grid.addWidget(virtual_list, 5, 1, 7, 1)
    grid.addWidget(custom_area, 12, 1, 3, 1)

    dialog = QDialog(window)
    dialog.setWindowTitle("Xenoteer Qt6 Fixture — Dialog")
    dialog.setObjectName("xenoteerQt6Dialog")
    dialog.setAccessibleName("Xenoteer Qt6 Dialog")
    dialog.setModal(False)
    dialog.setFixedSize(360, 140)
    dialog_layout = QVBoxLayout(dialog)
    dialog_layout.addWidget(QLabel("Stable transient window"))
    close = QPushButton("Stable close")
    close.clicked.connect(dialog.hide)
    dialog_layout.addWidget(close)

    window.show()
    dialog.show()
    payload = {
        "type": "qt6_fixture_ready",
        "pid": os.getpid(),
        "windows": [window.windowTitle(), dialog.windowTitle()],
    }
    if args.ready_file:
        args.ready_file.write_text(json.dumps(payload, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(payload, sort_keys=True), flush=True)
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
