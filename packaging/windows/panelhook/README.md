# The Windows panel hook

A DLL loaded into the apps opencomputeruse starts, so their open and save
dialogs go to the agent instead of the screen. It speaks the same protocol
as the macOS hook in `packaging/macos/panelhook`.

Off by default. Turn it on with `windows_panel_hook` in `config.json` or
`OCU_WINDOWS_PANEL_HOOK=1`. Without it, dialogs are answered on screen over
UI Automation (`crates/ocu-windows/src/dialog.rs`). The hook is only needed
for apps whose dialogs UI Automation cannot reach, or when no dialog may
appear at all.

## Loading

The app is created suspended inside its Job Object
(`crates/ocu-windows/src/launch.rs`). `crates/ocu-windows/src/inject.rs`
writes the hook's path into it and queues a `LoadLibraryW` APC on its
primary thread, which runs on resume, before the app's entry point. Nothing
in the app's image is replaced. 32-bit apps are skipped.

## What it hooks

- `CoCreateInstance` in the exe's import table. For `CLSID_FileOpenDialog`
  and `CLSID_FileSaveDialog` it wraps the real object so `Show` asks the
  agent and `GetResult`/`GetResults` return its answer.
- `GetOpenFileNameW` and `GetSaveFileNameW` from comdlg32, wrapped whole.

Delay-loaded imports are not patched, so those apps show their own dialog.

## Protocol

The pipe name arrives in `OCU_PANEL_PIPE`, which `DllMain` clears so child
processes do not inherit it. The hook connects once and, per dialog:

```
->  {"pid":N,"kind":"open"|"save","multiple":bool,"folders":bool}
<-  {"paths":["C:\\a.txt"]}      pick these
<-  {"paths":[]}                 cancel
```

A cancel returns `HRESULT_FROM_WIN32(ERROR_CANCELLED)`. No reply within 30
seconds, or no pipe, shows the real dialog. The agent's side is
`crates/ocu-windows/src/hook.rs`, which takes the pid from the pipe, not
the request.

## Caveats

- Endpoint protection may quarantine it, signed or not. Releases sign it
  when `WINDOWS_CERT_PFX_BASE64` is set.
- With Memory Integrity (HVCI) on, the hook may not load. The session then
  falls back to UI Automation.

## Building

Locally with mingw: `packaging/windows/panelhook/build.sh`, then point
`OCU_PANEL_HOOK` at the DLL. Releases build it with MSVC in
`.github/workflows/release.yml` and embed it in the exe through
`OCU_PANEL_HOOK_DLL` (an absolute path; see `crates/ocu-windows/build.rs`).
The agent unpacks it to `%LOCALAPPDATA%\opencomputeruse\panelhook\<version>`.
Both builds fail if the hook imports anything but Windows system DLLs.

## Untested

The C has been compiled but never run. Before shipping, test on Windows:
modern and legacy dialogs (pick, cancel, multiple), the agent killed
mid-dialog, the agent not running, a session ending mid-dialog, and
machines with Defender and HVCI.
