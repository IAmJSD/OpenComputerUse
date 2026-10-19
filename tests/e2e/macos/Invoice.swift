// A test app for tests/extract.rs: a window showing an invoice, whose
// values the test asks Clairic for.
//
// Usage: Invoice
import AppKit

let lines = [
    "Invoice number: INV-2291",
    "Northwind Supply",
    "Issued: 2026-10-02",
    "Due: 2026-11-01",
    "4 pallets at $250.00 each",
    "Shipping: $80.00",
    "Total due: $1,080.00",
]

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let window = NSWindow(
    contentRect: NSRect(x: 200, y: 200, width: 420, height: 320),
    styleMask: [.titled, .closable], backing: .buffered, defer: false)
window.title = "Invoice"

let stack = NSStackView()
stack.orientation = .vertical
stack.alignment = .leading
stack.spacing = 6
stack.edgeInsets = NSEdgeInsets(top: 16, left: 16, bottom: 16, right: 16)
for line in lines {
    stack.addArrangedSubview(NSTextField(labelWithString: line))
}
let buttons = NSStackView(views: ["Save draft", "Send invoice"].map {
    NSButton(title: $0, target: nil, action: nil)
})
buttons.orientation = .horizontal
stack.addArrangedSubview(buttons)
window.contentView = stack
window.makeKeyAndOrderFront(nil)
app.run()
