#!/usr/bin/env python3
"""A test app for tests/file_dialogs.rs: opens a window, then a moment later
an open or save dialog through GtkFileChooserNative (the portal when one is
offered), and writes what came back to a file.

Usage: dialog.py <open|save> <result file>
"""
import sys

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk  # noqa: E402

mode, out = sys.argv[1], sys.argv[2]

window = Gtk.Window(title="Dialog test")
window.set_default_size(360, 220)
window.connect("destroy", Gtk.main_quit)
window.show_all()


def ask():
    action = Gtk.FileChooserAction.SAVE if mode == "save" else Gtk.FileChooserAction.OPEN
    dialog = Gtk.FileChooserNative.new("Pick", window, action, None, None)
    if mode == "save":
        dialog.set_current_name("untitled.txt")
    accepted = dialog.run() == Gtk.ResponseType.ACCEPT
    with open(out, "w") as f:
        f.write((dialog.get_filename() or "") if accepted else "CANCELLED")
    Gtk.main_quit()
    return False


GLib.timeout_add(1000, ask)
Gtk.main()
