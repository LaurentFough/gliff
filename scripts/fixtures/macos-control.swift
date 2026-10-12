// Opt-in receiver for control_check. Run only during a bounded live test.
import AppKit

final class ControlFixture: NSObject, NSApplicationDelegate {
    var window: NSWindow?
    let view = NSTextView()
    var events: [[String: Any]] = []
    var saved: [[NSPasteboard.PasteboardType: Data]] = []
    var previous: NSRunningApplication?
    func applicationDidFinishLaunching(_ notification: Notification) {
        previous = NSWorkspace.shared.frontmostApplication
        for item in NSPasteboard.general.pasteboardItems ?? [] {
            var copy: [NSPasteboard.PasteboardType: Data] = [:]
            for type in item.types { if let data = item.data(forType: type) { copy[type] = data } }
            saved.append(copy)
        }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString("gliff-native-source", forType: .string)
        let menu = NSMenu()
        let edit = NSMenuItem(); menu.addItem(edit); edit.submenu = NSMenu(title: "Edit")
        edit.submenu?.addItem(withTitle: "Paste", action: #selector(NSTextView.paste(_:)), keyEquivalent: "v")
        NSApplication.shared.mainMenu = menu
        let screen = NSScreen.main!.frame
        let window = NSWindow(contentRect: NSRect(x: 80, y: screen.height - 480, width: 720, height: 380), styleMask: [.titled, .closable], backing: .buffered, defer: false)
        window.title = "Gliff control check — temporary"
        view.isRichText = false; view.font = NSFont.systemFont(ofSize: 20)
        window.contentView = view; window.makeKeyAndOrderFront(nil); window.makeFirstResponder(view)
        self.window = window
        NSApplication.shared.activate(ignoringOtherApps: true)
        NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp, .flagsChanged, .leftMouseDown, .leftMouseUp, .scrollWheel]) { [weak self] event in
            if let self = self, self.events.count < 100 {
                let keyboard = event.type == .keyDown || event.type == .keyUp || event.type == .flagsChanged
                self.events.append(["type": event.type.rawValue, "keycode": keyboard ? Int(event.keyCode) : -1, "shift": event.modifierFlags.contains(.shift), "command": event.modifierFlags.contains(.command), "scroll": event.type == .scrollWheel ? event.scrollingDeltaY : 0])
            }
            return event
        }
        Timer.scheduledTimer(withTimeInterval: 14, repeats: false) { [weak self] _ in
            guard let self = self else { return }
            let report: [String: Any] = ["text": self.view.string, "events": self.events]
            let data = try! JSONSerialization.data(withJSONObject: report)
            FileHandle.standardOutput.write(data)
            NSApplication.shared.terminate(nil)
        }
    }
    func applicationWillTerminate(_ notification: Notification) {
        let items = saved.map { values -> NSPasteboardItem in
            let item = NSPasteboardItem(); for (type, data) in values { item.setData(data, forType: type) }; return item
        }
        NSPasteboard.general.clearContents(); NSPasteboard.general.writeObjects(items)
        previous?.activate(options: [.activateIgnoringOtherApps])
    }
}
let app = NSApplication.shared
app.setActivationPolicy(.regular)
let delegate = ControlFixture()
app.delegate = delegate
withExtendedLifetime(delegate) { app.run() }
