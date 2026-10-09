---
name: opencomputeruse-remote
description: Operate desktop apps on another computer that runs OpenComputerUse, through its HTTP API, using the host names, URLs and keys kept in ~/.config/opencomputeruse/hosts.yaml. Starts apps there in the background, takes screenshots, reads accessibility trees, clicks, types and presses keys. Use when asked to do anything in an app on a remote computer.
---

# Driving a remote computer

Other computers run OpenComputerUse, which lets this device drive their apps over HTTP. Apps run in the background there; the computer's user keeps their own screen, pointer and keyboard. Each computer is a host in `~/.config/opencomputeruse/hosts.yaml`, so one file holds the keys for several.

## The hosts file

This file holds keys. Never print, cat, grep or `set -x` it, and never put a key in a command line, a message or a file. Use the functions below, which read the key themselves. If it is missing, ask the user to create it (the OpenComputerUse app's Generate Skill gives them the entry):

```yaml
hosts:
  work-mac:
    url: "http://work-mac.example.ts.net:8642"
    key: "ocu_..."
  studio:
    url: "http://studio.example.ts.net:8642"
    key: "ocu_..."
```

It is a small subset of YAML: a top-level `hosts:` map; each host a name with indented `url:` and `key:` values, bare or in single or double quotes (no escape sequences); `#` comments; later entries of the same name win. Keys may hold only letters, digits and `._~+/=-`; URLs must be plain `http://` or `https://`. Create it with `umask 077; mkdir -p ~/.config/opencomputeruse` and `chmod 600` on the file. It needs a POSIX shell, awk and curl (not available on Windows).

## Setting up the shell

Each shell command starts fresh, so save this once, with the folder made as above, to `~/.config/opencomputeruse/ocu.sh` (it holds no secrets, and a private folder keeps anyone else from swapping it) and begin every command that talks to a computer with `. ~/.config/opencomputeruse/ocu.sh`:

```sh
OCU_HOSTS="${OCU_HOSTS:-$HOME/.config/opencomputeruse/hosts.yaml}"
_ocu_entry() (
  awk -v host="${1-}" -v file="$OCU_HOSTS" '
    function unq(v,   q) {
      sub(/^[ \t]+/, "", v)
      q = substr(v, 1, 1)
      if (q == "\"" || q == "\047") { v = substr(v, 2); sub(q ".*$", "", v); return v }
      sub(/[ \t]+#.*$/, "", v); sub(/[ \t]+$/, "", v); return v
    }
    { sub(/\r$/, "") }
    /^[ \t]*(#|$)/ { next }
    /^hosts:[ \t]*(#.*)?$/ { inh = 1; hi = -1; next }
    /^[^ \t]/ { inh = 0; next }
    inh {
      match($0, /^[ \t]+/); ind = RLENGTH
      line = substr($0, ind + 1)
      if (hi < 0) hi = ind
      if (ind == hi) {
        name = line; sub(/:[ \t]*(#.*)?$/, "", name); name = unq(name)
        cur = name
        if (!(cur in seen)) { order[++n] = cur; seen[cur] = 1 }
        u[cur] = ""; k[cur] = ""
      } else if (cur != "") {
        f = line; sub(/:.*$/, "", f)
        v = line; sub(/^[^:]*:/, "", v)
        if (f == "url") u[cur] = unq(v)
        if (f == "key") k[cur] = unq(v)
      }
    }
    END {
      if (host == "") { for (i = 1; i <= n; i++) print order[i] "\t" u[order[i]]; exit 0 }
      if (!(host in seen)) { print "ocu: no host \"" host "\" in " file > "/dev/stderr"; exit 1 }
      if (u[host] !~ /^https?:\/\// || u[host] ~ /[ \t"\\\047]/ || k[host] !~ /^[A-Za-z0-9._~+\/=-]+$/) {
        print "ocu: host \"" host "\" needs a plain http(s) url and a key of letters, digits and ._~+/=-" > "/dev/stderr"; exit 1
      }
      printf "%s\t%s\n", u[host], k[host]
    }' "$OCU_HOSTS"
)
ocu_hosts() ( _ocu_entry )
ocu() (
  set -eu
  : "${2:?usage: ocu HOST TOOL [JSON]}"
  entry=$(_ocu_entry "${1:?usage: ocu HOST TOOL [JSON]}")
  url=${entry%"$(printf '\t')"*}
  key=${entry#*"$(printf '\t')"}
  body=${3-}; [ -n "$body" ] || body='{}'
  printf 'header = "Authorization: Bearer %s"\n' "$key" |
    curl -sS -K - -X POST "$url/v1/tools/$2" -H 'Content-Type: application/json' --data-binary "$body"
)
```

- `ocu_hosts` lists the hosts with their URLs. Run it to see which ones exist, and ask the user which to use when it is not clear.
- `ocu HOST TOOL [JSON]` calls one tool on HOST and prints the response. Never call `_ocu_entry`: it prints the key.

## Calling a tool

```sh
. ~/.config/opencomputeruse/ocu.sh
ocu work-mac start_session '{"app": "TextEdit"}'
```

The response is `{"content": [...], "isError": false}`. `content` holds a `text` item, and for actions and screenshots an `image` item: a base64 PNG of the app's window. To look at it, save it and open the file:

```sh
. ~/.config/opencomputeruse/ocu.sh
ocu work-mac screenshot '{"session_id": "SESSION_ID"}' > /tmp/ocu-response.json
jq -r '.content[] | select(.type == "text") | .text' /tmp/ocu-response.json
jq -r '.content[] | select(.type == "image") | .data' /tmp/ocu-response.json | base64 --decode > /tmp/ocu-screen.png
```

For arguments with apostrophes or quotes, pass the JSON as `"$(cat <<'EOF'
{"text": "it's here"}
EOF
)"`.

## How to work

1. `start_session` with the app (a name like "Safari", a bundle id, or a path) and keep the `session_id`.
2. Look with `screenshot` or `get_ui_tree`. Coordinates are points from the window's top-left, on the screenshot's pixel grid.
3. Act with `click`, `type_text`, `press_key`, `scroll` and the rest. Each action returns a fresh screenshot unless you pass `"screenshot": false`. Prefer element ids from the tree (`{"element": "e12"}`) over coordinates.
4. `end_session` when finished. Sessions also end if this device's key is regenerated or removed.

## Tools

Not every host offers every tool. A computer offers what its system has: `unlock_screen` only on a Mac; the phone, iOS simulator and Android emulator tools only when they are turned on in its settings (the simulator ones on a Mac, the emulator ones when the emulator is installed); and `run_recipe` once it is set up. A phone running the OpenComputerUse app offers only the session tools, for apps on that phone. A host answers a tool it doesn't have with an error.

### `start_session`

Start an app and get a session id for driving it. Use this, not other computer-use tools, for desktop apps: it is the one the user chose. Leave sessions in the background: clicking, typing, scrolling, menus and screenshots all work on a window behind the user's, without touching their screen, pointer or keyboard, so don't bring apps forward to use them. Set `foreground` only when something needs the app in front, such as the user asking to watch. On macOS `app` is a .app path, a bundle id (com.apple.TextEdit) or an app name ("TextEdit"); on Linux and Windows it is an executable path or a command on PATH. On Linux each session gets its own virtual X display. With `active_window: true` (macOS, Windows) and no `app`, it attaches to the window in front instead (skipping the app this conversation runs in), so the user can point you at a window by bringing it forward. Returns the session id and the app's windows. For phones, simulators and emulators use phone_start_session, ios_simulator_start_session or android_emulator_start_session.

  - `app` (string): The app to start. Required unless `active_window` is set.
  - `args` (array)
  - `env` (object)
  - `cwd` (string)
  - `new_instance` (boolean): macOS: start a separate instance even when the app is already running. Otherwise a running app is attached to, and left running when the session ends.
  - `active_window` (boolean): macOS and Windows: attach to the window in front (the topmost one not belonging to the app this client runs in) instead of starting `app`. It becomes the session's default window, and its app is left running when the session ends.
  - `foreground` (boolean): macOS and Windows: bring the app and its window to the front before every action. Rarely needed, since everything works in the background; use it when the user wants to watch, or an app ignores input while behind other windows. Default false. Linux sessions are always on their own virtual display.
  - `display_width` (integer): Linux: the virtual display's width. Default 1440.
  - `display_height` (integer): Linux: the virtual display's height. Default 900.

### `end_session`

End a session, quitting the app if the session started it. Sessions also end when this server exits.

  - `session_id` (string, required): The id start_session returned.

### `list_sessions`

List this client's live sessions.

### `list_windows`

List a session's windows, best first, with ids, titles and screen frames.

  - `session_id` (string, required): The id start_session returned.

### `screenshot`

Capture a session's window, even when it is covered by other windows. Coordinates are points from the window's top-left: the pixel grid of its screenshot.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `ui_tree` (boolean): Also return the accessibility tree. Default false.

### `get_ui_tree`

Read a window's accessibility tree: one line per element with an id (e12), role, name, value, frame in window coordinates and available actions. Element ids work with click, set_value and element_action until the next read.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `max_depth` (integer): Default 25.
  - `max_nodes` (integer): Default 1500.

### `click`

Click in a session's window without moving your real pointer or focus. Give x/y, or an element id. Coordinates are points from the window's top-left: the pixel grid of its screenshot.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `x` (number): Where to click x, in window coordinates.
  - `y` (number): Where to click y, in window coordinates.
  - `element` (string): An element id from the accessibility tree (e.g. "e12") to click instead of x/y. Uses the element's own press action, the most reliable way to click in a background window.
  - `button` (string): Default left. One of `left`, `right`, `middle`.
  - `count` (integer): 2 for a double click. Default 1.
  - `modifiers` (string): Keys held while clicking, e.g. "cmd" or "shift+alt".
  - `screenshot` (boolean): Return a screenshot of the window after the action. Default true.
  - `ui_tree` (boolean): Return the accessibility tree after the action. Default false.

### `move_mouse`

Move the session's pointer, for hover effects. Coordinates are points from the window's top-left: the pixel grid of its screenshot.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `x` (number, required): Where to move x, in window coordinates.
  - `y` (number, required): Where to move y, in window coordinates.
  - `screenshot` (boolean): Return a screenshot of the window after the action. Default true.
  - `ui_tree` (boolean): Return the accessibility tree after the action. Default false.

### `drag`

Press, drag and release. Coordinates are points from the window's top-left: the pixel grid of its screenshot.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `from_x` (number, required)
  - `from_y` (number, required)
  - `to_x` (number, required)
  - `to_y` (number, required)
  - `button` (string) One of `left`, `right`, `middle`.
  - `screenshot` (boolean): Return a screenshot of the window after the action. Default true.
  - `ui_tree` (boolean): Return the accessibility tree after the action. Default false.

### `scroll`

Scroll at a point. Coordinates are points from the window's top-left: the pixel grid of its screenshot.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `x` (number, required): Where to scroll x, in window coordinates.
  - `y` (number, required): Where to scroll y, in window coordinates.
  - `dx` (number): Pixels to scroll right (negative: left).
  - `dy` (number): Pixels to scroll down (negative: up).
  - `screenshot` (boolean): Return a screenshot of the window after the action. Default true.
  - `ui_tree` (boolean): Return the accessibility tree after the action. Default false.

### `type_text`

Type text into the window's focused field. Newlines press Return.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `text` (string, required)
  - `screenshot` (boolean): Return a screenshot of the window after the action. Default true.
  - `ui_tree` (boolean): Return the accessibility tree after the action. Default false.

### `press_key`

Press keys: chords joined with +, several separated by spaces. Modifiers: cmd (meta/win/super), ctrl, alt (option), shift. Named keys: enter, tab, escape, backspace, delete, space, up, down, left, right, home, end, pageup, pagedown, f1-f24. Examples: "cmd+s", "ctrl+shift+tab", "down down enter". On phones and simulators, also the device's buttons: home, back (Android), recents (Android), power, volume_up, volume_down.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `keys` (string, required)
  - `screenshot` (boolean): Return a screenshot of the window after the action. Default true.
  - `ui_tree` (boolean): Return the accessibility tree after the action. Default false.

### `set_value`

Set an element's value directly (a text field's contents, a slider's position) through accessibility. On a web page's dropdown (a PopUpButton) the value is the option's text, picked without opening the menu.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `element` (string, required): Element id from get_ui_tree.
  - `value` (string, required)
  - `screenshot` (boolean): Return a screenshot of the window after the action. Default true.
  - `ui_tree` (boolean): Return the accessibility tree after the action. Default false.

### `element_action`

Perform an accessibility action on an element: press, focus, showmenu, increment, decrement, confirm, cancel, raise, pick. The tree lists each element's actions.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `element` (string, required): Element id from get_ui_tree.
  - `action` (string): Default press.
  - `screenshot` (boolean): Return a screenshot of the window after the action. Default true.
  - `ui_tree` (boolean): Return the accessibility tree after the action. Default false.

### `choose_file`

Answer the open or save panel (file picker) the app is showing, without clicking through it: give `paths` to pick those files or folders (one path for a save panel: where to save), or leave `paths` empty to cancel. Actions say when a panel is waiting. macOS only.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `paths` (array): Absolute paths (or starting with ~). Several only where the panel lets you pick several. Empty or left out: cancel.
  - `screenshot` (boolean): Return a screenshot of the window after the action. Default true.
  - `ui_tree` (boolean): Return the accessibility tree after the action. Default false.

### `wait`

Wait for the app, then look again.

  - `session_id` (string, required): The id start_session returned.
  - `window_id` (integer): A window id from list_windows. Defaults to the app's main window.
  - `ms` (integer, required): Milliseconds, at most 60000.
  - `screenshot` (boolean): Return a screenshot of the window after the action. Default true.
  - `ui_tree` (boolean): Return the accessibility tree after the action. Default false.

### `permissions`

Show the OS permissions computer use needs and whether they are granted.

### `unlock_screen`

Unlock the Mac if its screen is locked, so sessions can keep working. Does nothing when already unlocked. Needs "Work while the Mac is locked" turned on in settings.

### `ios_simulator_list`

List this Mac's iOS simulators: name, UDID, iOS version and whether each is booted.

### `ios_simulator_apps`

List the apps installed on an iOS simulator, with the bundle ids ios_simulator_start_session takes. Boots it briefly if it isn't running.

  - `simulator` (string): The simulator's name ("iPhone 17 Pro") or UDID. Defaults to the running one, else the newest iPhone.

### `ios_simulator_start_session`

Start an app on an iOS simulator and get a session id. The simulator boots headless if it isn't running (shut down again when the session ends), so nothing appears on the Mac's screen, and the app is driven through WebDriverAgent (downloaded the first time). Then drive it with the same session tools as a desktop app (screenshot, get_ui_tree, click, type_text, press_key, scroll, drag, set_value, element_action). Coordinates are points, the grid of its screenshots; click is a tap, a right click a long press, drag a swipe, and scroll swipes the content.

  - `app` (string, required): The app's bundle id, such as com.apple.mobilesafari or com.apple.Preferences.
  - `simulator` (string): The simulator's name ("iPhone 17 Pro") or UDID. Defaults to the running one, else the newest iPhone.
  - `show_window` (boolean): Open the Simulator app's window for it, so the user can watch. Default false.
  - `args` (array): Launch arguments for the app.
  - `env` (object): Environment variables for the app.

### `android_emulator_list`

List the Android virtual devices (AVDs) and which are running, with their serials.

### `android_emulator_apps`

List the launchable apps on a running Android emulator: package and activity.

  - `emulator` (string): The virtual device's (AVD's) name, or a running emulator's serial (emulator-5554). Defaults to the running emulator, else the only AVD.

### `android_emulator_start_session`

Start an app on an Android emulator and get a session id. The emulator boots headless if it isn't running (shut down again when the session ends). The app runs on a private virtual display unless `main_display` is set. Then drive it with the same session tools as a desktop app (screenshot, get_ui_tree, click, type_text, press_key, scroll, drag, set_value, element_action). Coordinates are points, the grid of its screenshots; click is a tap, a right click a long press, drag a swipe, and scroll swipes the content.

  - `app` (string, required): A package (com.android.settings) or an activity (com.android.settings/.Settings).
  - `emulator` (string): The virtual device's (AVD's) name, or a running emulator's serial (emulator-5554). Defaults to the running emulator, else the only AVD.
  - `show_window` (boolean): Show the emulator's window when it boots. Default false.
  - `main_display` (boolean): Run the app on the device's own screen instead of a private virtual display, for apps that misbehave there. Default false.
  - `args` (array): Extra `am start` arguments, such as ["-d", "https://example.com"] for a link.

### `phone_list`

List the phones and tablets connected to this computer: Android devices over adb (USB or wireless debugging), and iPhones and iPads paired with this Mac. Says what a device still needs, such as accepting USB debugging or Developer Mode.

### `phone_apps`

List the apps on a connected phone: packages and activities on Android, bundle ids on iOS.

  - `phone` (string): The phone's id, serial or name from phone_list. Defaults to the only connected phone.

### `phone_start_session`

Start an app on a connected phone or tablet and get a session id. Android apps run on a private virtual display, so the phone's own screen is left alone (unless `main_display` is set). iPhones and iPads are driven through WebDriverAgent, which is built and signed with your Xcode account the first time (a few minutes), and the app runs on the device's screen; keep it unlocked. Then drive it with the same session tools as a desktop app (screenshot, get_ui_tree, click, type_text, press_key, scroll, drag, set_value, element_action). Coordinates are points, the grid of its screenshots; click is a tap, a right click a long press, drag a swipe, and scroll swipes the content.

  - `app` (string, required): Android: a package or activity. iOS: a bundle id.
  - `phone` (string): The phone's id, serial or name from phone_list. Defaults to the only connected phone.
  - `main_display` (boolean): Android: run on the phone's own screen instead of a private virtual display. Default false.
  - `args` (array): Android: extra `am start` arguments. iOS: launch arguments.
  - `env` (object): iOS: environment variables for the app.

### `run_recipe`

Run a fixed sequence of UI steps in a session without you in the loop. Each step names its target in plain words; a fast decision model (TypeSafe Jev or Cloudflare Clef, chosen in the OpenComputerUse settings) matches that description to an element of the accessibility tree and the step acts on it. For straight-line chores with no decisions: 'click the address bar', 'type "example.com" into the address bar', 'press enter'. Stops at the first step it cannot place confidently and reports where it got to, with a screenshot. Steps are strings ("click <target>", "double click <target>", "right click <target>", "hover <target>", "type \"<text>\" into <target>", "type \"<text>\"", "set <target> to \"<value>\"", "press <keys>", "scroll down|up [in <target>]", "wait <ms>") or objects ({"click": target}, {"type": text, "into": target}, {"press": keys}, {"set_value": value, "on": target}, {"scroll": "down", "in": target}, {"wait": ms}).

  - `session_id` (string, required)
  - `window_id` (integer)
  - `steps` (array, required)
  - `min_confidence` (number): Override the configured confidence threshold (0-1).

