#!/usr/bin/env python3
"""Minimal GTK3 AT-SPI fixture with stable application and control names."""

import argparse
from pathlib import Path

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--ready-file", type=Path)
    args = parser.parse_args()

    GLib.set_prgname("xenoteer-atspi-fixture")
    GLib.set_application_name("Xenoteer AT-SPI Fixture")
    window = Gtk.Window(title="Xenoteer AT-SPI Fixture")
    window.set_default_size(420, 160)
    window.connect("destroy", Gtk.main_quit)

    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
    box.set_border_width(12)
    button = Gtk.Button(label="Xenoteer Probe Button")
    button.get_accessible().set_name("Xenoteer Probe Button")
    entry = Gtk.Entry()
    entry.set_placeholder_text("Xenoteer Probe Entry")
    entry.get_accessible().set_name("Xenoteer Probe Entry")
    box.pack_start(button, True, True, 0)
    box.pack_start(entry, True, True, 0)
    window.add(box)
    window.show_all()

    if args.ready_file:
        args.ready_file.write_text("ready\n", encoding="utf-8")
    print("READY Xenoteer AT-SPI Fixture", flush=True)
    Gtk.main()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
