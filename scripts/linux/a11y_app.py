#!/usr/bin/env python3
"""A window with a text field and a button, for the accessibility smoke
tests: pressing the button puts the field's text in the window title.

    python3 scripts/linux/a11y_app.py [3|4]   # the GTK version, default 4
"""
import sys

import gi

version = sys.argv[1] if len(sys.argv) > 1 else "4"
gi.require_version("Gtk", f"{version}.0")
from gi.repository import GLib, Gtk  # noqa: E402


def build(window):
    window.set_title("OCU accessibility")
    window.set_default_size(480, 240)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
    for side in ("top", "bottom", "start", "end"):
        getattr(box, f"set_margin_{side}")(24)
    entry = Gtk.Entry()
    entry.set_placeholder_text("Your name")
    button = Gtk.Button(label="Greet")
    button.connect("clicked", lambda _: window.set_title("greeted:" + entry.get_text()))
    if version == "4":
        entry.update_property([Gtk.AccessibleProperty.LABEL], ["Name"])
        box.append(entry)
        box.append(button)
        window.set_child(box)
    else:
        entry.get_accessible().set_name("Name")
        box.pack_start(entry, False, False, 0)
        box.pack_start(button, False, False, 0)
        window.add(box)


# A plain window, not a Gtk.Application, which would want a session bus.
if version == "4":
    loop = GLib.MainLoop()
    window = Gtk.Window()
    window.connect("close-request", lambda _: loop.quit())
    build(window)
    window.present()
    loop.run()
else:
    window = Gtk.Window()
    window.connect("destroy", Gtk.main_quit)
    build(window)
    window.show_all()
    Gtk.main()
