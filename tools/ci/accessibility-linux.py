"""Disposable GTK target for explicitly dispatched AT-SPI qualification."""
import os
import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk

window = Gtk.Window(title="DevWorks AT-SPI Qualification")
window.set_default_size(320, 180)
button = Gtk.Button(label="Qualify Action")
button.get_accessible().set_name("Qualify Action")


def pressed(_button):
    with open(os.environ["DEVWORKS_ACCESSIBILITY_MARKER"], "w", encoding="utf-8") as marker:
        marker.write("pressed")
    GLib.timeout_add(500, Gtk.main_quit)


button.connect("clicked", pressed)
window.add(button)
window.connect("destroy", Gtk.main_quit)
window.show_all()
window.present()
Gtk.main()
