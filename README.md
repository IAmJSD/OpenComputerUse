<p align="center"><img src="assets/icon.svg" width="128" height="128" alt="OpenComputerUse icon"></p>

# OpenComputerUse

Computer use for agents that runs in the background, as an MCP server.
Start a session with an app and get back a session id. Drive the app with
that id, then end the session. The app works behind your other windows,
and your pointer, keyboard focus and frontmost app stay where they are.
Sessions end when the MCP server exits, including when it is killed.

## Install

On macOS, install with Homebrew:

```sh
brew tap IAmJSD/OpenComputerUse https://github.com/IAmJSD/OpenComputerUse
brew trust iamjsd/opencomputeruse
brew install --cask opencomputeruse
```

Homebrew won't load casks from a third-party tap until you trust it, which is
what `brew trust` is for. The cask puts `OpenComputerUse.app` in `/Applications` and links the
`opencomputeruse` command into Homebrew's `bin`. The app updates itself, so
`brew upgrade` leaves it alone. Or download `OpenComputerUse.dmg` from the
[latest release](https://github.com/IAmJSD/OpenComputerUse/releases/latest).
Linux and Windows releases have plain binaries of the MCP server.

Then add it to your client (see [Install into a client](#install-into-a-client)).

## Tools

| Tool | What it does |
| --- | --- |
| `start_session` | Start an app (a `.app` path, bundle id or name on macOS; an executable elsewhere) and return a session id and its windows; `foreground: true` (macOS, Windows) brings it to the front for every action instead of keeping it in the background, and `active_window: true` (macOS, Windows) attaches to the window in front instead of starting an app |
| `end_session`, `list_sessions`, `list_windows` | Manage sessions |
| `screenshot` | Capture a session window, even a covered one; `ui_tree: true` adds the accessibility tree |
| `get_ui_tree` | One line per element: `[e12] Button "Save" @(x,y wxh) actions=press` |
| `click`, `move_mouse`, `drag`, `scroll` | Pointer actions at window coordinates, or `click` with `element: "e12"` |
| `type_text`, `press_key` | Text, and chords such as `cmd+s` or `ctrl+shift+tab enter` |
| `set_value`, `element_action` | Set an element's value (including a dropdown's option), or run press, focus, showmenu, increment and similar actions |
| `choose_file` | Answer the file picker the app is showing (macOS): the paths to pick or save to, or none to cancel. Actions say when one is waiting |
| `wait` | Let the app catch up |
| `run_recipe` | Run a fixed list of steps with a decision model (see below) |
| `permissions` | What the OS needs granted, and whether it is |
| `phone_list`, `phone_apps`, `phone_start_session` | Phones and tablets connected to this computer (Android over adb; iPhones and iPads on macOS), when turned on |
| `ios_simulator_list`, `ios_simulator_apps`, `ios_simulator_start_session` | iOS simulators (macOS only), when turned on |
| `android_emulator_list`, `android_emulator_apps`, `android_emulator_start_session` | Android emulators (when the emulator is installed), when turned on |

Every action returns a fresh screenshot unless you pass `screenshot: false`.
Pass `ui_tree: true` to also get the tree. Coordinates are points from the
window's top-left, the same grid as its screenshot.

The `*_start_session` tools for devices return an ordinary session id, and
every other tool works on it (see [Phones, tablets, simulators and
emulators](#phones-tablets-simulators-and-emulators-cratesocu-mobile)).

## Recipes

`run_recipe` runs straight-line chores without the calling model in the
loop:

```json
{ "session_id": "…", "steps": [
  "click the address bar",
  "type \"example.com\" into the address bar",
  "press enter",
  { "click": "the More information link" }
] }
```

For each step, the runner sends the window's interactive elements to a
decision model as the options of one `choice` question. It acts on the
chosen element when the model is confident enough. There is no branching,
and the model generates no text. A step it cannot place stops the recipe and
returns a screenshot and the tree so the caller can take over. Two models
are supported, chosen in the app's settings:

- **TypeSafe Jev** (`api.typesafe.ai`, model `jev-latest`)
- **Cloudflare Clef / Clef-flash** on Workers AI (`@cf/cloudflare/clef-flash`)

## Platforms

Each platform implements the `Platform` and `Session` traits in
`crates/ocu-core`. Everything above them is shared: session ids, ownership,
cleanup, the MCP tools and recipes.

### macOS (`crates/ocu-macos`)

- **Launch:** apps open without activating. Your app's windows are put back
  on top, so the session window sits behind them.
- **Screenshots:** ScreenCaptureKit captures the window itself, so covered
  windows capture as they look.
- **Tree and element actions:** the Accessibility API, which works on
  background windows.
- **Pointer and keys:** posted to the app's process through SkyLight. The
  app is first told its window is active, without being raised; this
  "focus without raise" approach comes from yabai and trycua/cua.
- **File pickers:** `choose_file` answers them, most smoothly first:
  - Chromium browsers started with their own `--user-data-dir` get a
    DevTools pipe (`--remote-debugging-pipe`, so no port is opened). A
    page's `<input type=file>` then never shows a panel; its files are set
    over the pipe. `showOpenFilePicker` and kin still open the panel.
  - Firefox started with its own `-profile` opens WebDriver BiDi on a
    local port, and OCU takes its only session at once. File pickers are
    held back and answered with `input.setFiles`.
  - Everything else shows AppKit's panel, which is drawn by a separate
    process (`openAndSavePanelService`). Input goes to that process, the
    path goes in through the panel's Go to sheet, and screenshots compose
    the panel as the screen draws it.
- **Dropdowns:** `set_value` picks a web `<select>`'s option without
  opening it, and an app's pop-up button's through its menu, which shows
  for a moment.

The MCP server is a thin client. The **OpenComputerUse app** owns the
sessions, holds the Accessibility and Screen Recording permissions, and
draws a halo and a gliding cursor over the window being driven. It is built
with GPUI (the `IAmJSD/gpui` fork). The MCP server starts the app through
LaunchServices when needed, so the app keeps its own permissions whichever
client started the server. Opening the app shows its window:
permissions, one-click install into Claude Code, Claude Desktop, Codex or OpenCode, live sessions, and
recipe settings.

```sh
CODESIGN_IDENTITY="Developer ID Application: …" scripts/bundle-macos.sh   # universal (arm64 + x86_64)
cp -R dist/OpenComputerUse.app /Applications/ && open /Applications/OpenComputerUse.app
```

The icon is `assets/icon.svg`; `packaging/macos/icon.sh` rebuilds the `.icns` from it.

Sign the bundle with a real identity. An ad hoc signature changes on every
build, and macOS then asks for the permissions again.

### Linux (`crates/ocu-linux`)

- **Display:** each session gets a private Xvfb display with its own cookie.
  The app starts on it via `DISPLAY` and `XAUTHORITY`, with Wayland
  disabled.
- **Input and screenshots:** input goes through XTEST; screenshots read the
  framebuffer, so menus and popups are included.
- **Cleanup:** Xvfb and the app get `PR_SET_PDEATHSIG`, so they die with
  the server.
- **Where Xvfb is found:** `$OCU_XVFB`, next to the binary, or on `PATH`.
- **Not yet:** the accessibility tree (AT-SPI), so element actions and
  recipes are unavailable on Linux for now.
- **Watching a session:** connect a VNC server to its display, for example
  `x11vnc -display :N -auth <xauthority>`. The display and auth file are in
  the session details.

```sh
docker build -f scripts/linux/Dockerfile -t ocu-linux . && docker run --rm ocu-linux
```

### Windows (`crates/ocu-windows`)

- **Launch:** apps start suspended inside a kill-on-close Job Object, so
  their whole process tree dies with the server. They are shown without
  activating and sent to the back.
- **Screenshots:** `PrintWindow` with full-content rendering.
- **Input:** window messages posted to the control under the point, or to
  the focused control.
- **Tree and element actions:** UI Automation.

### Phones, tablets, simulators and emulators (`crates/ocu-mobile`)

Off by default: none of these tools are offered until **Phones, simulators
and emulators** is turned on in the app (elsewhere, `"mobile": true` in the
settings file or `OCU_MOBILE=1`). Clients are told when the tool list
changes, and sessions already running carry on when it is turned off.

Sessions on devices take the same tools as desktop ones. Coordinates are
points (iOS) or density-independent pixels (Android), and screenshots are
scaled to match. A click is a tap, a right click a long press, `drag` a
swipe, and `scroll` swipes the content by `dx`/`dy`. `press_key` also takes
the device's buttons: `home`, `back` and `recents` (Android), `power`,
`volume_up`, `volume_down`. There is no hovering, so `move_mouse` fails.

On macOS the app owns device sessions, as it does desktop ones. Elsewhere
the MCP server does. Downloads and emulator logs go in the settings
directory's `mobile` folder.

**Android** (any desktop) is driven over adb. It needs Android's
platform-tools, found through `$ANDROID_HOME`, `$ANDROID_SDK_ROOT`, Android
Studio's and Homebrew's SDK locations, then `PATH` (or `$OCU_ADB`).

- **Phones:** turn on USB debugging (or wireless debugging) and accept the
  prompt. `phone_list` says what a device still needs.
- **Emulators:** the `android_emulator_*` tools are listed when the emulator
  is installed. A session on an emulator that isn't running boots its AVD
  headless and shuts it down when the last session on it ends.
- **The helper:** sessions push OpenComputerUse's APK
  (`OpenComputerUse.apk` from the matching release, fetched once) to
  `/data/local/tmp` and run its shell helper with `app_process`, the way
  scrcpy runs its server. Nothing is installed. The helper gives the app a
  private virtual display, so the phone's own screen, pointer and keyboard
  are left alone. It also reads the accessibility tree through
  `UiAutomation`, injects input into that display, and types any Unicode
  text into the focused field. This needs Android 11 (API 30) or later.
  `main_display: true` runs the app on the device's own screen instead, for
  apps that refuse a secondary display. `home` and `recents` aren't
  available on a private display, since it has no launcher.
- **Without the helper** (an older Android, or no APK), sessions use the
  device's own screen with adb's `input`, `screencap` and
  `uiautomator dump`. That only types ASCII.
- **Launching:** `app` is a package, which starts its launcher activity, or
  an activity such as `com.android.settings/.Settings`. `args` are passed to
  `am start`, for example `["-d", "https://example.com"]`.

**iOS** (macOS, with Xcode) is driven through Appium's
[WebDriverAgent](https://github.com/appium/WebDriverAgent), the XCUITest
server; `OCU_WDA_VERSION` picks another release.

- **Simulators:** the `ios_simulator_*` tools use WebDriverAgent's prebuilt
  simulator runner. The app bundles it, made universal, signed, notarized
  on its own (the notary service doesn't look inside the tarball) and packed
  with xz by `scripts/wda-sim-runner.sh` (3.8 MB, with WebDriverAgent's BSD
  licence beside it), and unpacks it once, so nothing is downloaded. A build
  outside the app downloads it from WebDriverAgent's release once. A session boots the
  simulator headless if it isn't running, so nothing appears on screen.
  `show_window: true` opens Simulator.app. The simulator shuts down when the
  last session that booted it ends.
- **iPhones and iPads:** the device has to be paired and in Developer Mode.
  The first session builds WebDriverAgent from source with `xcodebuild` and
  signs it with your Apple development team, taken from your "Apple
  Development" certificate or `OCU_IOS_TEAM`; Xcode must be signed in to that
  team. That takes a few minutes once. It's built again by itself when its
  provisioning profile expires (after a week on a free Apple account) or
  doesn't yet include the device, or when the device refuses its signature. It's installed and launched with
  `devicectl` and reached over CoreDevice's tunnel, so USB or the local
  network both work. The app runs on the device's screen, so keep the device
  unlocked.
- **Launching:** `app` is a bundle id, and `args` and `env` are the app's
  launch arguments and environment. The `*_apps` tools list bundle ids.

Developer builds use `dist/OpenComputerUse.apk` from this checkout
(`android/build.sh`), or `OCU_ANDROID_APK`.

### An Android phone as a host (`android/`)

The same APK is also an app that makes a phone a host by itself, with no
computer: other devices drive the phone's apps over HTTP, the way they drive
a computer's [Other devices](#other-devices-http) server. It uses the same
API, keys, skill and hosts file. An accessibility service reads the screen
and taps, swipes and types, and a foreground service serves on port 8642.

Install `OpenComputerUse.apk` from the release, open it, turn it on in
Accessibility settings, turn on **Serve over HTTP**, and tap **Generate
key…** for each device that will drive the phone. A phone offers the
session tools only, for apps on itself. The app's own screen, where keys are
made, can't be reached over HTTP. See [android/README.md](android/README.md),
including setup over adb.

The skill is the same from every host, phones included.
`opencomputeruse skill` prints it, and the app ships that text as
`android/res/raw/skill.md`. The tests fail when the two differ.

## Install into a client

From the app's window, or:

```sh
opencomputeruse install claude          # claude mcp add --scope user opencomputeruse -- <path> mcp
opencomputeruse install claude-desktop  # an "mcpServers" entry in claude_desktop_config.json
opencomputeruse install codex           # codex mcp add opencomputeruse -- <path> mcp
opencomputeruse install opencode        # an "mcp" entry in ~/.config/opencode/opencode.json(c)
opencomputeruse install kimi            # an "mcpServers" entry in ~/.kimi-code/mcp.json
opencomputeruse clients                 # which clients run this copy
```

Claude Desktop reads its config when it starts, so quit and reopen it after
installing. Kimi means Kimi Code (`kimi`); it keeps its config in
`$KIMI_CODE_HOME`, `~/.kimi-code` by default. The legacy `kimi-cli` is not supported. A client whose entry runs another copy (an old build, or the app before it
moved) shows as "points elsewhere"; installing again points it here.

For other clients, use `{ "command": "<path to opencomputeruse>", "args": ["mcp"] }`.

## Other devices (HTTP)

Off by default. Turn on **Serve over HTTP** in the app (or the local MCP
server's `http_server` tool) and other devices can drive this computer
through an HTTP API on port 8642, usually over Tailscale. While it is on, it
starts again at login, so it survives reboots.

Each device needs a key. **Generate Skill** asks for the device's name and the
URL it reaches this computer at (this computer's Tailscale name by default),
and shows the key once, in a dialog with two tabs:

- **Prompt for an agent**: one prompt to paste into an agent on the device
  (Claude Code, Codex, OpenCode, …). It holds the skill and this computer's
  hosts-file entry, and asks the agent to install the one and add the other.
- **Skill file**: the hosts-file entry and the skill, to put in place by
  hand. The skill is the same for every computer, so a device needs it only
  once; each computer it drives adds an entry.

Keys are stored only as hashes, so that is the one place a key appears.
Devices are listed in the app with **Regenerate Key** and **Remove**, and in
the local MCP server as `list_devices`, `generate_skill`, `regenerate_key`
and `remove_device`. Regenerating or removing a key ends that device's
sessions. None of this management is reachable over HTTP.

### The skill and the hosts file

The skill (`opencomputeruse-remote`) holds no key. It teaches the agent to read
host names, URLs and keys from `~/.config/opencomputeruse/hosts.yaml` on the
device that drives the others, so one file serves several computers:

```yaml
hosts:
  work-mac:
    url: "http://work-mac.example.ts.net:8642"
    key: "ocu_..."
  studio:
    url: "http://studio.example.ts.net:8642"
    key: "ocu_..."
```

Only this small shape is read: a `hosts:` map, each host with `url:` and
`key:` values (bare, or in single or double quotes), `#` comments, and the
last entry of a name winning. Create the file with `umask 077` and
`chmod 600`. Keys may hold only letters, digits and `._~+/=-`. The skill has the agent save its
shell helper to `~/.config/opencomputeruse/ocu.sh`; `ocu HOST TOOL [JSON]` reads the key itself and gives it to
curl on stdin, so it never shows up in a command line or the agent's
transcript. It needs a POSIX shell, awk and curl, so not Windows.

The API:

- `POST /v1/tools/<tool>` with JSON arguments returns `{content, isError}`,
  the same as an MCP tool call. `GET /v1/tools` lists the tools.
- `POST /mcp` is MCP over HTTP:
  `claude mcp add --transport http <name> <url>/mcp --header "Authorization: Bearer <key>"`.
- Every request needs `Authorization: Bearer <key>`, except `GET /health`.

There is no TLS, so use it over Tailscale or another trusted network. On
Linux and Windows, `opencomputeruse serve` runs the server, and
`serve --install` starts it at login.

It listens on every network adapter unless you limit it, for example so it
is reachable over Tailscale but not on a coffee shop's Wi-Fi. Untick
**Every network adapter to every host** in the app, pass
`serve --listen-on`, or set `http.listen_on` in the settings file to a list
of:

- an adapter, such as `en0`, or `tailscale` for whichever adapter has this
  computer's Tailscale address,
- an address, such as `192.168.1.5`,
- a range, such as `192.168.1.0/24`.

The server follows these as addresses come and go, and waits while none of
them has one: `opencomputeruse serve --listen-on tailscale` listens once
Tailscale connects.

## Settings

The settings file is at `opencomputeruse config-path`. On macOS it is
edited from the app. Elsewhere, edit it by hand or set `TYPESAFE_API_KEY`,
`CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_API_TOKEN`, `OCU_RECIPE_PROVIDER` and
`OCU_MOBILE` (`1` turns the mobile tools on, `0` off).
Set `OCU_LOG=debug` for logs on stderr. The macOS agent logs to
`~/Library/Application Support/OpenComputerUse/agent.log`.

## Releasing

Bump `version` in `Cargo.toml`, commit, and push a matching tag (`v0.2.0`).
`.github/workflows/release.yml` builds the signed universal app
(`OpenComputerUse.zip` for the updater, `OpenComputerUse.dmg` for first
installs), plain Linux and Windows binaries of the MCP server, and
`OpenComputerUse.apk` for Android, then publishes them as a GitHub release. The app checks for releases daily and from
its menu, and installs them in place when they are signed by the same team.

After the release is published, bump `version` and `sha256` in
`Casks/opencomputeruse.rb` to the new `OpenComputerUse.zip`
(`shasum -a 256 OpenComputerUse.zip`) so new Homebrew installs get it.

The macOS job signs and notarizes with these repository secrets, and builds
unsigned without them: `MACOS_CERT_P12_BASE64`, `MACOS_CERT_P12_PASSWORD`
(a Developer ID Application certificate), `APPLE_ID`,
`APPLE_APP_SPECIFIC_PASSWORD` and `APPLE_TEAM_ID`.

The Android job builds `OpenComputerUse.apk` with `android/build.sh` and
signs it with `ANDROID_KEYSTORE_BASE64`, `ANDROID_KEYSTORE_PASSWORD` and
`ANDROID_KEY_ALIAS`. Without them it signs with a throwaway key, which is
fine for the helper but stops the phone app updating in place.

## Development

```sh
cargo test                                     # core, recipes and the updater
cargo run -p ocu-macos --example smoke         # drive TextEdit directly
python3 scripts/mcp_smoke.py                   # drive it through the MCP server
```

The widget kit in `src/agent/ui` comes from Schist (MIT; see
`LICENSE-SCHIST`).

## License

MIT, © 2026 Astrid Gealer. See [LICENSE](LICENSE).
