#!/usr/bin/env python3
"""Drive and screenshot the real SeeMyMac window (X11/Xwayland), e.g. on a headless compositor.

Setup (no monitor needed; Fedora/GNOME):
    dbus-run-session -- mutter --headless --wayland --virtual-monitor 1440x900 --wayland-display=wl-test &
    export DISPLAY=:N XAUTHORITY=<the .mutter-Xwaylandauth file of that Xwayland>   # see `pgrep -a Xwayland`
    WAYLAND_DISPLAY= XDG_CONFIG_HOME=/tmp/cfg SEEMYMAC_DEMO=1 ./target/debug/see_my_mac &
    .venv/bin/python -m pip install python-xlib
    .venv/bin/python tools/ui_driver.py key n Control_L        # Ctrl+N
    .venv/bin/python tools/ui_driver.py click 740 280
    .venv/bin/python tools/ui_driver.py type "Studio Mac"
    .venv/bin/python tools/ui_driver.py shot /tmp/out.png      # needs ImageMagick `import`

Coordinates are relative to the app window. Never point DISPLAY at your real desktop's
Xwayland: this injects real mouse and keyboard events.
"""
import sys, time, subprocess, os
from Xlib import X, XK, display
from Xlib.ext import xtest
d = display.Display()
root = d.screen().root
def app_window():
    for c in root.query_tree().children:
        try:
            if c.get_wm_name() == "SeeMyMac": return c
        except Exception: pass
    sys.exit("no app window")
w = app_window()
def origin():
    t = w.translate_coords(root, 0, 0)
    return -t.x, -t.y
def move(x, y):
    ox, oy = origin(); xtest.fake_input(d, X.MotionNotify, 0, X.CurrentTime, root, ox + x, oy + y); d.sync(); time.sleep(0.15)
def click(x, y, button=1):
    move(x, y)
    xtest.fake_input(d, X.ButtonPress, button); d.sync(); time.sleep(0.08)
    xtest.fake_input(d, X.ButtonRelease, button); d.sync(); time.sleep(0.4)
def key(name, mods=()):
    ks = XK.string_to_keysym(name); kc = d.keysym_to_keycode(ks)
    for m in mods: xtest.fake_input(d, X.KeyPress, d.keysym_to_keycode(XK.string_to_keysym(m)))
    xtest.fake_input(d, X.KeyPress, kc); d.sync(); time.sleep(0.04)
    xtest.fake_input(d, X.KeyRelease, kc)
    for m in reversed(mods): xtest.fake_input(d, X.KeyRelease, d.keysym_to_keycode(XK.string_to_keysym(m)))
    d.sync(); time.sleep(0.15)
def typ(text):
    names = {" ": "space", ".": "period", ":": "colon", "-": "minus", "_": "underscore", "@": "at", "/": "slash", ",": "comma", "~": "asciitilde"}
    for ch in text:
        n = names.get(ch, ch)
        if ch.isupper(): key(n, ["Shift_L"])
        else: key(n)
def shot(path):
    time.sleep(0.5)
    subprocess.run(["import", "-window", hex(w.id), path], check=True)
cmd, *a = sys.argv[1:]
if cmd == "click": click(int(a[0]), int(a[1]), int(a[2]) if len(a) > 2 else 1)
elif cmd == "key": key(a[0], a[1:])
elif cmd == "type": typ(" ".join(a))
elif cmd == "shot": shot(a[0])
