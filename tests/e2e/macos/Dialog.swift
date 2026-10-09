// A test app for tests/file_dialogs.rs: opens a window, then a moment later
// an open or save panel, and writes what came back to a file.
//
// Usage: Dialog <open|save> <result file>
import AppKit

let args = CommandLine.arguments
let mode = args.count > 1 ? args[1] : "open"
let out = args.count > 2 ? args[2] : "/dev/null"

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let window = NSWindow(
    contentRect: NSRect(x: 200, y: 200, width: 360, height: 220),
    styleMask: [.titled], backing: .buffered, defer: false)
window.title = "Dialog test"
window.makeKeyAndOrderFront(nil)

DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
    let panel: NSSavePanel
    if mode == "save" {
        panel = NSSavePanel()
        panel.nameFieldStringValue = "untitled.txt"
    } else {
        panel = NSOpenPanel()
    }
    let result = panel.runModal() == .OK ? (panel.url?.path ?? "") : "CANCELLED"
    try? result.write(toFile: out, atomically: true, encoding: .utf8)
    exit(0)
}
app.run()
