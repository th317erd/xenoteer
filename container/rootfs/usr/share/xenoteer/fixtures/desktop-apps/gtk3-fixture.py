#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Deterministic GTK3/X11 fixture with stable AT-SPI names and window variants."""

import argparse
import json
import os
from pathlib import Path

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--ready-file", type=Path)
    args = parser.parse_args()

    if os.environ.get("NO_AT_BRIDGE") == "1":
        raise SystemExit("NO_AT_BRIDGE=1 disables required accessibility")

    GLib.set_prgname("xenoteer-gtk3-fixture")
    GLib.set_application_name("Xenoteer GTK3 Fixture")

    window = Gtk.Window(title="Xenoteer GTK3 Fixture — Main")
    window.set_wmclass("xenoteer-gtk3-fixture", "XenoteerFixture")
    window.set_default_size(900, 700)
    window.set_position(Gtk.WindowPosition.CENTER)
    window.connect("destroy", Gtk.main_quit)
    window.get_accessible().set_name("Xenoteer GTK3 Main Window")

    outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
    menubar = Gtk.MenuBar()
    menu_root = Gtk.MenuItem(label="Fixture menu")
    menu_root.get_accessible().set_name("Stable Menu")
    submenu = Gtk.Menu()
    menu_action = Gtk.MenuItem(label="Nested menu action")
    menu_action.get_accessible().set_name("Nested Menu Action")
    submenu.append(menu_action)
    menu_root.set_submenu(submenu)
    menubar.append(menu_root)
    toolbar = Gtk.Toolbar()
    tool_action = Gtk.ToolButton.new(None, "Toolbar action")
    tool_action.get_accessible().set_name("Stable Toolbar Action")
    toolbar.insert(tool_action, -1)
    outer.pack_start(menubar, False, False, 0)
    outer.pack_start(toolbar, False, False, 0)
    grid = Gtk.Grid(column_spacing=12, row_spacing=8, margin=12)
    heading = Gtk.Label(label="Xenoteer deterministic GTK3 fixture")
    heading.set_xalign(0)
    heading.get_accessible().set_name("Fixture Heading")
    entry = Gtk.Entry(text="editable fixture text")
    entry.set_placeholder_text("Stable entry")
    entry.get_accessible().set_name("Stable Entry")
    protected = Gtk.Entry(text="fixture-secret")
    protected.set_visibility(False)
    protected.get_accessible().set_name("Protected Entry")
    checkbox = Gtk.CheckButton(label="Stable checkbox")
    checkbox.set_active(True)
    checkbox.get_accessible().set_name("Stable Checkbox")
    button = Gtk.Button(label="Stable button")
    button.get_accessible().set_name("Stable Button")
    double_click = Gtk.Button(label="Double-click target")
    double_click.get_accessible().set_name("Double-click Target")
    disabled = Gtk.Button(label="Disabled button")
    disabled.set_sensitive(False)
    disabled.get_accessible().set_name("Disabled Button")
    combo = Gtk.ComboBoxText()
    for item in ("Alpha", "Beta", "Gamma"):
        combo.append_text(item)
    combo.set_active(1)
    combo.get_accessible().set_name("Stable Choice")
    radio_alpha = Gtk.RadioButton.new_with_label_from_widget(None, "Alpha radio")
    radio_beta = Gtk.RadioButton.new_with_label_from_widget(radio_alpha, "Beta radio")
    radio_alpha.set_active(True)
    radio_alpha.get_accessible().set_name("Stable Radio Alpha")
    radio_beta.get_accessible().set_name("Stable Radio Beta")
    switch = Gtk.Switch()
    switch.set_active(True)
    switch.get_accessible().set_name("Stable Switch")
    slider = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 100, 1)
    slider.set_value(40)
    slider.get_accessible().set_name("Stable Slider")
    spinner = Gtk.SpinButton.new_with_range(0, 100, 1)
    spinner.set_value(7)
    spinner.get_accessible().set_name("Stable Spin")
    text_view = Gtk.TextView()
    text_view.get_buffer().set_text("editable multiline fixture text")
    text_view.set_size_request(280, 70)
    text_view.get_accessible().set_name("Stable Text Area")
    fonts = Gtk.Label(label="Latin — العربية — 中文 — e\u0301 — 😀")
    fonts.set_xalign(0)
    fonts.get_accessible().set_name("Font Coverage")

    controls = (
        heading, entry, protected, checkbox, radio_alpha, radio_beta, switch,
        slider, spinner, combo, button, double_click, disabled, text_view, fonts,
    )
    for row, widget in enumerate(controls):
        grid.attach(widget, 0, row, 1, 1)

    notebook = Gtk.Notebook()
    notebook.get_accessible().set_name("Stable Tabs")
    notebook.append_page(Gtk.Label(label="First tab content"), Gtk.Label(label="First tab"))
    notebook.append_page(Gtk.Label(label="Second tab content"), Gtk.Label(label="Second tab"))
    store = Gtk.ListStore(str)
    for index in range(64):
        store.append([f"Virtual row {index:02d}"])
    tree = Gtk.TreeView(model=store)
    tree.get_accessible().set_name("Stable Virtual List")
    tree.append_column(Gtk.TreeViewColumn("Rows", Gtk.CellRendererText(), text=0))
    scroller = Gtk.ScrolledWindow()
    scroller.set_size_request(340, 300)
    scroller.add(tree)
    drawing = Gtk.DrawingArea()
    drawing.set_size_request(240, 120)
    drawing.set_tooltip_text("Stable drawing tooltip")
    drawing.get_accessible().set_name("Stable Custom Area")
    grid.attach(notebook, 1, 0, 1, 3)
    grid.attach(scroller, 1, 3, 1, 8)
    grid.attach(drawing, 1, 11, 1, 3)
    outer.pack_start(grid, True, True, 0)
    window.add(outer)

    dialog = Gtk.Dialog(
        title="Xenoteer GTK3 Fixture — Dialog",
        transient_for=window,
        modal=False,
        destroy_with_parent=True,
    )
    dialog.set_default_size(360, 140)
    dialog.get_accessible().set_name("Xenoteer GTK3 Dialog")
    dialog.add_button("Stable close", Gtk.ResponseType.CLOSE)
    dialog.get_content_area().add(Gtk.Label(label="Stable transient window"))
    dialog.connect("response", lambda widget, _response: widget.hide())

    window.show_all()
    dialog.show_all()
    payload = {
        "type": "gtk3_fixture_ready",
        "pid": os.getpid(),
        "windows": [window.get_title(), dialog.get_title()],
    }
    if args.ready_file:
        args.ready_file.write_text(json.dumps(payload, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(payload, sort_keys=True), flush=True)
    Gtk.main()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
