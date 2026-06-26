#!/usr/bin/env python3
r"""
bos_driver.py - local UI-automation bridge for driving ComfortClick on the
machine where it runs. Lets an agent (Claude Code, running locally) actually
SEE the screen (real screenshots) and, when armed, click/type in the
Configurator. This is the "crawl + click ComfortClick" surface.

Two classes of command:
  OBSERVE  (always allowed, never changes anything)
    windows                 list visible top-level window titles
    screenshot [opts]       capture the screen (or a window) to a PNG
    tree   [opts]           dump the UI control tree (names + positions) as JSON
    find --name TEXT        locate a control by name -> its center + box

  ACT  (refused unless --arm, because this drives a live building)
    click  X Y | --name T   left-click a point or a found control's center
    dblclick X Y | --name T
    rightclick X Y
    move   X Y
    type   "text"
    key    NAME[,NAME...]   e.g. enter / tab / ctrl,s

Setup:
    pip install pyautogui pillow uiautomation
      - pyautogui+pillow  -> screenshot / click / type / key   (required)
      - uiautomation      -> windows / tree / find by name     (optional but
                             strongly recommended: lets clicks target named
                             controls instead of blind pixels)

Safety:
  * OBSERVE commands change nothing and are always allowed.
  * ACT commands abort unless you pass --arm (or set BOS_DRIVER_ARM=1). Without
    it they print what they WOULD do (dry-run) and exit non-zero.
  * pyautogui failsafe is ON: slam the mouse into a screen corner to abort.
  * Point this at a TEST / standby bOS instance before any live system.

Typical agent loop:
    python bos_driver.py screenshot --out shot.png        # I look at shot.png
    python bos_driver.py tree --window Configurator        # I learn what's clickable
    python bos_driver.py click --name "Tasks" --arm        # I click a named node
    python bos_driver.py screenshot --out shot.png         # I verify the result
"""
import sys, os, json, argparse, time

# Window titles / node names contain non-ASCII (e.g. Swedish å, box-drawing
# glyphs); the Windows console defaults to cp1252 and would crash on them.
try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except Exception:
    pass

ARMED_ENV = os.environ.get("BOS_DRIVER_ARM") == "1"

# ---- optional backends, imported lazily / gracefully -----------------------
def _pyautogui():
    try:
        import pyautogui
        pyautogui.FAILSAFE = True
        pyautogui.PAUSE = 0.15
        return pyautogui
    except Exception as e:
        sys.exit(f"pyautogui/pillow required for this command. pip install pyautogui pillow\n({e})")

def _uia():
    try:
        import uiautomation as auto
        return auto
    except Exception:
        return None


# ---- OBSERVE ---------------------------------------------------------------
def cmd_windows(a):
    auto = _uia()
    if auto:
        root = auto.GetRootControl()
        out = []
        for w in root.GetChildren():
            name = (w.Name or "").strip()
            if name:
                r = w.BoundingRectangle
                out.append({"title": name, "class": w.ClassName,
                            "rect": [r.left, r.top, r.right, r.bottom]})
        print(json.dumps(out, ensure_ascii=False, indent=2))
        return
    # fallback: pygetwindow via pyautogui
    try:
        import pygetwindow as gw
        titles = [t for t in gw.getAllTitles() if t.strip()]
        print(json.dumps(titles, ensure_ascii=False, indent=2))
    except Exception:
        sys.exit("Install 'uiautomation' (recommended) or 'pygetwindow' to list windows.")

def _find_window(auto, title):
    """Return the top-level window control whose Name contains `title`."""
    root = auto.GetRootControl()
    for w in root.GetChildren():
        if title.lower() in (w.Name or "").lower():
            return w
    return None

def cmd_screenshot(a):
    pg = _pyautogui()
    region = None
    if a.window:
        auto = _uia()
        if not auto:
            sys.exit("--window needs 'uiautomation' installed.")
        w = _find_window(auto, a.window)
        if not w:
            sys.exit(f"No window matching: {a.window}")
        r = w.BoundingRectangle
        region = (r.left, r.top, r.right - r.left, r.bottom - r.top)
    elif a.region:
        region = tuple(int(x) for x in a.region.split(","))
    img = pg.screenshot(region=region) if region else pg.screenshot()
    out = a.out or "bos_screen.png"
    img.save(out)
    print(json.dumps({"saved": os.path.abspath(out), "size": list(img.size),
                      "region": list(region) if region else None}))

def _walk(ctrl, depth, max_depth, out, only_interactive):
    try:
        name = (ctrl.Name or "").strip()
        ctype = ctrl.ControlTypeName
        r = ctrl.BoundingRectangle
        box = [r.left, r.top, r.right, r.bottom]
        interactive = ctype in ("ButtonControl", "TreeItemControl", "MenuItemControl",
                                "TabItemControl", "ListItemControl", "EditControl",
                                "CheckBoxControl", "ComboBoxControl", "HyperlinkControl")
        if name and (not only_interactive or interactive):
            out.append({"name": name, "type": ctype, "box": box,
                        "center": [(r.left + r.right) // 2, (r.top + r.bottom) // 2]})
    except Exception:
        pass
    if depth < max_depth:
        for ch in ctrl.GetChildren():
            _walk(ch, depth + 1, max_depth, out, only_interactive)

def cmd_tree(a):
    auto = _uia()
    if not auto:
        sys.exit("'tree' needs 'uiautomation'. pip install uiautomation")
    root = _find_window(auto, a.window) if a.window else auto.GetRootControl()
    if not root:
        sys.exit(f"No window matching: {a.window}")
    out = []
    _walk(root, 0, a.depth, out, a.interactive)
    print(json.dumps(out, ensure_ascii=False, indent=1))

def cmd_find(a):
    auto = _uia()
    if not auto:
        sys.exit("'find' needs 'uiautomation'. pip install uiautomation")
    root = _find_window(auto, a.window) if a.window else auto.GetRootControl()
    if not root:
        sys.exit(f"No window matching: {a.window}")
    out = []
    _walk(root, 0, a.depth, out, False)
    q = a.name.lower()
    hits = [c for c in out if q in c["name"].lower()]
    print(json.dumps(hits[:a.limit], ensure_ascii=False, indent=1))


# ---- ACT (gated) -----------------------------------------------------------
def _require_arm(a, what):
    if a.arm or ARMED_ENV:
        return True
    print(json.dumps({"dry_run": True, "would": what,
                      "hint": "re-run with --arm (or BOS_DRIVER_ARM=1) to actually do it."}))
    sys.exit(2)

def _resolve_point(a):
    if a.name:
        auto = _uia()
        if not auto:
            sys.exit("--name needs 'uiautomation'.")
        root = _find_window(auto, a.window) if a.window else auto.GetRootControl()
        out = []
        _walk(root, 0, a.depth, out, False)
        hits = [c for c in out if a.name.lower() in c["name"].lower()]
        if not hits:
            sys.exit(f"No control matching: {a.name}")
        if len(hits) > 1:
            sys.exit("Ambiguous --name (" + str(len(hits)) +
                     " matches); use exact text or click X Y:\n" +
                     json.dumps([h["name"] for h in hits[:10]], ensure_ascii=False))
        return hits[0]["center"], hits[0]["name"]
    if a.x is None or a.y is None:
        sys.exit("Give X Y or --name.")
    return [a.x, a.y], None

def cmd_click(a, kind="click"):
    pt, name = _resolve_point(a)
    _require_arm(a, {kind: pt, "name": name})
    pg = _pyautogui()
    pg.moveTo(pt[0], pt[1])
    if kind == "dblclick":
        pg.doubleClick()
    elif kind == "rightclick":
        pg.rightClick()
    else:
        pg.click()
    print(json.dumps({"did": kind, "at": pt, "name": name}))

def cmd_move(a):
    pt, name = _resolve_point(a)
    _pyautogui().moveTo(pt[0], pt[1])
    print(json.dumps({"moved": pt, "name": name}))

def cmd_type(a):
    _require_arm(a, {"type": a.text})
    _pyautogui().typewrite(a.text, interval=0.02)
    print(json.dumps({"typed": a.text}))

def cmd_key(a):
    keys = [k.strip() for k in a.keys.split(",") if k.strip()]
    _require_arm(a, {"hotkey": keys})
    pg = _pyautogui()
    if len(keys) > 1:
        pg.hotkey(*keys)
    else:
        pg.press(keys[0])
    print(json.dumps({"keys": keys}))

def cmd_export(a):
    """Macro: click the export menu, click the export item, fill the Save
    dialog, confirm. Labels are parameters (--menu/--item) so they match what
    `tree` actually shows - this encodes the steps, not a guessed UI."""
    plan = {"click_menu": a.menu, "then_click": a.item, "save_path": a.path}
    _require_arm(a, {"export": plan})
    auto = _uia()
    if not auto:
        sys.exit("'export' needs 'uiautomation' to find the menu/item by name.")
    pg = _pyautogui()

    def click_named(text):
        # re-walk the tree each time so newly-opened menu items are seen
        root = _find_window(auto, a.window) if a.window else auto.GetRootControl()
        out = []
        _walk(root, 0, a.depth, out, False)
        exact = [c for c in out if c["name"].lower() == text.lower()]
        sub = [c for c in out if text.lower() in c["name"].lower()]
        hits = exact or sub
        if not hits:
            sys.exit(f"export: control not found: {text!r} "
                     f"(open the menu in the UI or check the label with `tree`)")
        c = hits[0]
        pg.moveTo(c["center"][0], c["center"][1]); pg.click()
        return c["name"]

    log = [("menu", click_named(a.menu))]
    time.sleep(0.6)
    log.append(("item", click_named(a.item)))
    time.sleep(1.0)
    # Save dialog: select any existing filename, type the target path, confirm.
    pg.hotkey("ctrl", "a")
    pg.typewrite(a.path, interval=0.01)
    time.sleep(0.3)
    pg.press("enter")
    log.append(("saved_to", a.path))
    print(json.dumps({"export": log, "verify": "run `screenshot` and check the file exists"},
                     ensure_ascii=False))


def main():
    ap = argparse.ArgumentParser(description="ComfortClick UI-automation bridge (safe-by-default).")
    ap.add_argument("--arm", action="store_true", help="allow actions that click/type (live!)")
    sub = ap.add_subparsers(dest="cmd", required=True)

    sub.add_parser("windows")

    s = sub.add_parser("screenshot")
    s.add_argument("--out"); s.add_argument("--window"); s.add_argument("--region",
        help="x,y,w,h")

    for nm in ("tree", "find"):
        s = sub.add_parser(nm)
        s.add_argument("--window"); s.add_argument("--depth", type=int, default=40)
        if nm == "tree":
            s.add_argument("--interactive", action="store_true",
                           help="only clickable controls (buttons/tree items/...)")
        else:
            s.add_argument("--name", required=True); s.add_argument("--limit", type=int, default=15)

    for nm in ("click", "dblclick", "rightclick", "move"):
        s = sub.add_parser(nm)
        s.add_argument("x", type=int, nargs="?"); s.add_argument("y", type=int, nargs="?")
        s.add_argument("--name"); s.add_argument("--window"); s.add_argument("--depth", type=int, default=40)

    s = sub.add_parser("type"); s.add_argument("text")
    s = sub.add_parser("key"); s.add_argument("keys", help="comma-separated, e.g. ctrl,s")

    s = sub.add_parser("export")
    s.add_argument("--path", required=True, help="where to save the .bos")
    s.add_argument("--menu", default="File", help="top menu label to click first")
    s.add_argument("--item", default="Export", help="export item label to click next")
    s.add_argument("--window"); s.add_argument("--depth", type=int, default=40)

    a = ap.parse_args()
    {
        "windows": cmd_windows, "screenshot": cmd_screenshot, "tree": cmd_tree, "find": cmd_find,
        "click": lambda a: cmd_click(a, "click"),
        "dblclick": lambda a: cmd_click(a, "dblclick"),
        "rightclick": lambda a: cmd_click(a, "rightclick"),
        "move": cmd_move, "type": cmd_type, "key": cmd_key, "export": cmd_export,
    }[a.cmd](a)


if __name__ == "__main__":
    main()
