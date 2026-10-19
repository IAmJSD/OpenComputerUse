#!/usr/bin/env python3
"""A test app for tests/extract.rs: a window showing an invoice, whose
values the test asks Clairic for. Usage: invoice.py
"""
import gi

gi.require_version("Gtk", "3.0")
from gi.repository import Gtk  # noqa: E402

LINES = [
    "Invoice number: INV-2291",
    "Northwind Supply",
    "Issued: 2026-10-02",
    "Due: 2026-11-01",
    "4 pallets at $250.00 each",
    "Shipping: $80.00",
    "Total due: $1,080.00",
]

window = Gtk.Window(title="Invoice")
window.set_default_size(420, 320)
window.connect("destroy", Gtk.main_quit)
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6, margin=16)
for line in LINES:
    label = Gtk.Label(label=line, xalign=0)
    box.pack_start(label, False, False, 0)
buttons = Gtk.Box(spacing=8)
for title in ["Save draft", "Send invoice"]:
    buttons.pack_start(Gtk.Button(label=title), False, False, 0)
box.pack_start(buttons, False, False, 8)
window.add(box)
window.show_all()
Gtk.main()
