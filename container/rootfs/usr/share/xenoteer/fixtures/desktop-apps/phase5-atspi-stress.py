#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Bounded GTK3 AT-SPI stress fixture for Phase 5 live acceptance."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path

import gi

gi.require_version("Atk", "1.0")
gi.require_version("Gtk", "3.0")
from gi.repository import Atk, GLib, Gtk  # noqa: E402


MAX_NODES = 12_000
MAX_DEPTH = 128
MAX_EVENT_BURST = 100_000
MAX_EVENT_START_DELAY_MS = 30_000
MAX_OVERSIZED_NAME_BYTES = 128 * 1024
EVENT_BATCH = 32


def bounded_integer(name: str, minimum: int, maximum: int):
    def parse(value: str) -> int:
        parsed = int(value)
        if parsed < minimum or parsed > maximum:
            raise argparse.ArgumentTypeError(
                f"{name} must be between {minimum} and {maximum}"
            )
        return parsed

    return parse


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--ready-file", type=Path)
    parser.add_argument(
        "--nodes", type=bounded_integer("nodes", 1, MAX_NODES), default=4_096
    )
    parser.add_argument(
        "--depth", type=bounded_integer("depth", 1, MAX_DEPTH), default=80
    )
    parser.add_argument(
        "--event-burst",
        type=bounded_integer("event-burst", 0, MAX_EVENT_BURST),
        default=0,
    )
    parser.add_argument(
        "--event-start-delay-ms",
        type=bounded_integer(
            "event-start-delay-ms", 0, MAX_EVENT_START_DELAY_MS
        ),
        default=1_500,
    )
    parser.add_argument("--event-trigger-file", type=Path)
    parser.add_argument("--event-complete-file", type=Path)
    parser.add_argument("--silent-name-trigger-file", type=Path)
    parser.add_argument("--silent-name-complete-file", type=Path)
    parser.add_argument(
        "--oversized-name-bytes",
        type=bounded_integer(
            "oversized-name-bytes", 0, MAX_OVERSIZED_NAME_BYTES
        ),
        default=0,
    )
    args = parser.parse_args()

    if os.environ.get("NO_AT_BRIDGE") == "1":
        raise SystemExit("NO_AT_BRIDGE=1 disables required accessibility")

    GLib.set_prgname("xenoteer-phase5-atspi-stress")
    GLib.set_application_name("Xenoteer Phase5 AT-SPI Stress")

    window = Gtk.Window(title="Xenoteer Phase5 AT-SPI Stress — Main")
    window.set_wmclass("xenoteer-phase5-atspi-stress", "XenoteerFixture")
    window.set_default_size(1_000, 720)
    window.connect("destroy", Gtk.main_quit)
    window.get_accessible().set_name("Phase5 Stress Main Window")

    outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6, margin=8)
    standard = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
    button = Gtk.Button(label="Phase5 stable action")
    button.get_accessible().set_name("Phase5 Stable Button")
    entry = Gtk.Entry(text="phase5 editable text")
    entry.get_accessible().set_name("Phase5 Stable Entry")
    protected = Gtk.Entry(text="phase5-secret-never-print")
    protected.set_visibility(False)
    protected.get_accessible().set_name("Phase5 Protected Entry")
    standard.pack_start(button, False, False, 0)
    standard.pack_start(entry, True, True, 0)
    standard.pack_start(protected, True, True, 0)
    outer.pack_start(standard, False, False, 0)

    silent_name = Gtk.Label(label="controlled missing-event source")
    silent_name_accessible = silent_name.get_accessible()
    silent_name_accessible.set_name("Phase5 Silent Event Source")
    outer.pack_start(silent_name, False, False, 0)

    custom = Gtk.DrawingArea()
    custom.set_size_request(320, 80)
    custom.set_tooltip_text("physical-only custom surface")
    custom.get_accessible().set_name("Phase5 Incomplete Custom Canvas")
    outer.pack_start(custom, False, False, 0)

    malformed = Gtk.Label(label="adversarial relation fixture")
    malformed_accessible = malformed.get_accessible()
    malformed_accessible.set_name("Phase5 Malformed Self Relation")
    if not malformed_accessible.add_relationship(
        Atk.RelationType.NODE_CHILD_OF, malformed_accessible
    ):
        raise SystemExit("ATK rejected the required cyclic relation fixture")
    outer.pack_start(malformed, False, False, 0)

    if args.oversized_name_bytes:
        oversized = Gtk.Label(label="oversized accessible-name fixture")
        oversized.get_accessible().set_name("M" * args.oversized_name_bytes)
        outer.pack_start(oversized, False, False, 0)

    panes = Gtk.Paned.new(Gtk.Orientation.HORIZONTAL)
    # Materialize the large accessibility surface. GtkTreeView deliberately
    # omits virtual rows from Cache.GetItems even while reporting thousands of
    # live children, which tests lazy-toolkit fallback rather than the bounded
    # bulk-cache/query path this fixture is meant to exercise.
    large_list = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
    large_list.get_accessible().set_name("Phase5 Large Materialized List")
    for index in range(args.nodes):
        name = f"Phase5 Large Row {index:05d}"
        row = Gtk.Label(label=name, xalign=0)
        row.get_accessible().set_name(name)
        large_list.pack_start(row, False, False, 0)
    large_scroll = Gtk.ScrolledWindow()
    large_scroll.add_with_viewport(large_list)
    panes.pack1(large_scroll, resize=True, shrink=False)

    deep_root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
    deep_root.get_accessible().set_name("Phase5 Deep Tree")
    parent = deep_root
    for depth in range(args.depth):
        name = f"Phase5 Deep Node {depth:03d}"
        node = Gtk.Label(label=name, xalign=0)
        node.get_accessible().set_name(name)
        parent.pack_start(node, False, False, 0)
        child = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        parent.pack_start(child, False, False, 0)
        parent = child
    deep_scroll = Gtk.ScrolledWindow()
    deep_scroll.add_with_viewport(deep_root)
    panes.pack2(deep_scroll, resize=True, shrink=False)
    outer.pack_start(panes, True, True, 0)

    flood_status = Gtk.Label(label="event flood idle")
    flood_status.get_accessible().set_name("Phase5 Flood Status")
    churn_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
    outer.pack_start(flood_status, False, False, 0)
    outer.pack_start(churn_box, False, False, 0)
    window.add(outer)
    window.show_all()

    flood_state: dict[str, object] = {"emitted": 0, "child": None}

    def emit_event_batch() -> bool:
        emitted = int(flood_state["emitted"])
        for _ in range(min(EVENT_BATCH, args.event_burst - emitted)):
            previous = flood_state["child"]
            if isinstance(previous, Gtk.Widget):
                churn_box.remove(previous)
            child = Gtk.Button(label="transient")
            child.get_accessible().set_name(f"Phase5 Transient {emitted:05d}")
            churn_box.pack_start(child, False, False, 0)
            child.show()
            flood_state["child"] = child
            emitted += 1
            flood_status.get_accessible().set_name(f"Phase5 Flood Status {emitted:05d}")
        flood_state["emitted"] = emitted
        if emitted >= args.event_burst:
            flood_status.set_text("event flood complete")
            flood_status.get_accessible().set_name("Phase5 Flood Complete")
            if args.event_complete_file is not None:
                args.event_complete_file.write_text(
                    json.dumps({"emitted": emitted}, sort_keys=True) + "\n",
                    encoding="utf-8",
                )
            return False
        return True

    if args.event_burst:
        def start_event_flood() -> bool:
            GLib.timeout_add(1, emit_event_batch)
            return False

        if args.event_trigger_file is None:
            GLib.timeout_add(args.event_start_delay_ms, start_event_flood)
        else:
            def wait_for_event_trigger() -> bool:
                if not args.event_trigger_file.exists():
                    return True
                return start_event_flood()

            GLib.timeout_add(25, wait_for_event_trigger)
    elif args.event_trigger_file is not None or args.event_complete_file is not None:
        raise SystemExit("event trigger/completion files require --event-burst")

    if (args.silent_name_trigger_file is None) != (
        args.silent_name_complete_file is None
    ):
        raise SystemExit("silent-name trigger/completion files must be supplied together")
    if args.silent_name_trigger_file is not None:
        def wait_for_silent_name_trigger() -> bool:
            if not args.silent_name_trigger_file.exists():
                return True
            # ATK bridges the GObject accessible-name notification to the
            # AT-SPI PropertyChange signal. Freeze that one production
            # notification before changing the real toolkit property: a direct
            # Accessible.Name read sees the new value, while the daemon mirror
            # can learn it only through its ordinary bounded exact reconciler.
            silent_name_accessible.freeze_notify()
            silent_name_accessible.set_name("Phase5 Silent Event Changed")
            args.silent_name_complete_file.write_text(
                json.dumps({"changed": True}, sort_keys=True) + "\n",
                encoding="utf-8",
            )
            return False

        GLib.timeout_add(25, wait_for_silent_name_trigger)

    payload = {
        "type": "phase5_atspi_stress_ready",
        "pid": os.getpid(),
        "nodes": args.nodes,
        "depth": args.depth,
        "event_burst": args.event_burst,
        "event_start_delay_ms": args.event_start_delay_ms,
        "oversized_name_bytes": args.oversized_name_bytes,
        "silent_name_change": args.silent_name_trigger_file is not None,
    }
    if args.ready_file:
        args.ready_file.write_text(
            json.dumps(payload, sort_keys=True) + "\n", encoding="utf-8"
        )
    print(json.dumps(payload, sort_keys=True), flush=True)
    Gtk.main()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
