#!/usr/bin/env python3
"""Exercise production RemoteView input handling without posting OS input."""
import subprocess
import sys
import tempfile
from pathlib import Path


def main():
    if sys.platform != "darwin":
        sys.exit("Run this check on macOS.")
    source = Path(__file__).resolve().parents[1] / "crates/gliff-macos/native/Gliff.swift"
    text = source.read_text().split("let mode = ProcessInfo", 1)[0]
    old = '''func event(_ value: [String: Any]) {
    if let data = try? JSONSerialization.data(withJSONObject: value) { packet(2, data) }
}'''
    assert old in text, "cannot locate bridge event sink"
    text = text.replace(old, '''var observedEvents: [[String: Any]] = []
func event(_ value: [String: Any]) { observedEvents.append(value) }''')
    text = text.replace('e?.post(tap: .cghidEventTap)', 'if let e = e { observedNativeEvents.append(e) }')
    text = 'var observedNativeEvents: [CGEvent] = []\n' + text
    text += '''
func keyEvent(_ code: UInt16, _ flags: NSEvent.ModifierFlags = [], _ type: NSEvent.EventType = .keyDown) -> NSEvent {
    NSEvent.keyEvent(with: type, location: .zero, modifierFlags: flags, timestamp: 0, windowNumber: 0, context: nil, characters: "", charactersIgnoringModifiers: "", isARepeat: false, keyCode: code)!
}
let view = RemoteView(frame: NSRect(x: 0, y: 0, width: 100, height: 100))
view.keyDown(with: keyEvent(0))
view.flagsChanged(with: keyEvent(56, .shift, .flagsChanged))
precondition(observedEvents.isEmpty, "keys escaped the picture boundary")
view.hovering = true
view.flagsChanged(with: keyEvent(57, .capsLock, .flagsChanged))
view.flagsChanged(with: keyEvent(57, [], .flagsChanged))
precondition(observedEvents.count == 4, "Caps Lock did not pulse on both transitions")
precondition(observedEvents.map { $0["pressed"] as! Bool } == [true, false, true, false])
observedEvents.removeAll()
view.keyDown(with: keyEvent(0))
view.flagsChanged(with: keyEvent(56, .shift, .flagsChanged))
view.buttons.insert(0)
view.keyDown(with: keyEvent(53, .shift))
precondition(view.released && view.pressed.isEmpty && view.buttons.isEmpty, "hotkey left held input")
precondition(observedEvents.filter { $0["type"] as? String == "key" && $0["pressed"] as? Bool == false }.count == 2)
precondition(observedEvents.contains { $0["type"] as? String == "button" && $0["pressed"] as? Bool == false })
precondition(!observedEvents.contains { $0["keycode"] as? UInt16 == 53 }, "release hotkey reached host")
observedEvents.removeAll()
view.keyDown(with: keyEvent(1))
precondition(observedEvents.isEmpty, "released viewer forwarded a key")
view.released = false
let imageBytes = Data(repeating: 0, count: 8)
view.image = CGImage(width: 2, height: 1, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: 8, space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipFirst.rawValue), provider: CGDataProvider(data: imageBytes as CFData)!, decode: nil, shouldInterpolate: false, intent: .defaultIntent)
view.hovering = true
view.keyDown(with: keyEvent(0))
observedEvents.removeAll()
let outside = NSEvent.mouseEvent(with: .mouseMoved, location: NSPoint(x: 1, y: 1), modifierFlags: [], timestamp: 0, windowNumber: 0, context: nil, eventNumber: 0, clickCount: 0, pressure: 0)!
view.mouseMoved(with: outside)
precondition(!view.hovering && view.pressed.isEmpty, "leaving the picture did not release keys")
precondition(observedEvents.count == 1 && observedEvents[0]["pressed"] as? Bool == false)
observedEvents.removeAll()
view.keyDown(with: keyEvent(1))
precondition(observedEvents.isEmpty, "letterbox key reached the host")
let input = Input()
input.capsLock = false
input.handle(["type": "key", "keycode": UInt16(57), "pressed": true])
input.handle(["type": "key", "keycode": UInt16(57), "pressed": true])
input.handle(["type": "key", "keycode": UInt16(57), "pressed": false])
input.handle(["type": "key", "keycode": UInt16(0), "pressed": true])
precondition(observedNativeEvents.allSatisfy { $0.flags.contains(.maskAlphaShift) }, "host lost Caps Lock state")
observedNativeEvents.removeAll()
input.handle(["type": "key", "keycode": UInt16(57), "pressed": true])
input.handle(["type": "key", "keycode": UInt16(57), "pressed": false])
input.handle(["type": "key", "keycode": UInt16(0), "pressed": false])
precondition(observedNativeEvents.allSatisfy { !$0.flags.contains(.maskAlphaShift) }, "host did not turn Caps Lock off")
print("INPUT PASS: picture boundary, Caps Lock client/host state, Shift+Esc and held-input release")
'''
    with tempfile.TemporaryDirectory(prefix="gliff-input-check-") as folder:
        swift = Path(folder, "main.swift")
        binary = Path(folder, "input-check")
        swift.write_text(text)
        subprocess.run(["xcrun", "swiftc", "-O", "-swift-version", "5", str(swift), "-o", str(binary)], check=True, timeout=60)
        subprocess.run([str(binary)], check=True, timeout=10)


if __name__ == "__main__":
    main()
