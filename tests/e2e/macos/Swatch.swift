// A test app for tests/live_view.rs: a window filled with red, which turns
// blue when clicked and red again when clicked again.
//
// Usage: Swatch
import AppKit

final class SwatchView: NSView {
    var blue = false

    // Clicks reach it while its window is in the background.
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func mouseDown(with event: NSEvent) {
        blue.toggle()
        needsDisplay = true
    }

    override func draw(_ dirtyRect: NSRect) {
        (blue
            ? NSColor(srgbRed: 0, green: 0, blue: 1, alpha: 1)
            : NSColor(srgbRed: 1, green: 0, blue: 0, alpha: 1)).setFill()
        bounds.fill()
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let window = NSWindow(
    contentRect: NSRect(x: 200, y: 200, width: 360, height: 280),
    styleMask: [.titled, .closable], backing: .buffered, defer: false)
window.title = "Swatch"
window.contentView = SwatchView()
window.makeKeyAndOrderFront(nil)
app.run()
