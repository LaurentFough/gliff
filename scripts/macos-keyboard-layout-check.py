#!/usr/bin/env python3
"""Read real Mac layouts and the production change handler without selecting one."""
import platform
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
    assert old in text
    text = text.replace(old, '''var observedLayouts: [[String: Any]] = []
func event(_ value: [String: Any]) { observedLayouts.append(value) }''')
    text += '''
_ = NSApplication.shared
let original = keyboardLayout()!["source"] as! String
let german = keyboardLayout("com.apple.keylayout.German")!
let us = keyboardLayout("com.apple.keylayout.US")!
func level(_ layout: [String: Any], _ code: Int, _ index: Int = 0) -> [String: Any] {
    let key = (layout["keys"] as! [[String: Any]]).first { $0["keycode"] as? Int == code }!
    return (key["levels"] as! [[String: Any]])[index]
}
precondition(level(german, 16)["text"] as? String == "z")
precondition(level(german, 6)["text"] as? String == "y")
precondition(level(german, 41)["text"] as? String == "ö")
precondition(level(german, 33)["text"] as? String == "ü")
precondition(level(german, 39)["text"] as? String == "ä")
precondition(level(german, 37, 2)["text"] as? String == "@", "Option-L lost the Mac symbol")
precondition(level(german, 24)["dead"] as? Bool == true, "Acute accent lost its dead-key state")
precondition(level(us, 16)["text"] as? String == "y")
precondition(level(us, 6)["text"] as? String == "z")
let delegate = App()
delegate.keyboardLayoutChanged(nil)
precondition(observedLayouts.count == 1 && observedLayouts[0]["type"] as? String == "keyboard_layout")
precondition(observedLayouts[0]["source"] as? String == original)
precondition(keyboardLayout()!["source"] as? String == original, "Reading a fixture changed the user's layout")
print("KEYBOARD LAYOUT PASS: real German/US positions, umlauts, Option, dead keys and layout-change handler; input source unchanged")
'''
    with tempfile.TemporaryDirectory(prefix="gliff-keyboard-check-") as folder:
        swift = Path(folder, "main.swift")
        binary = Path(folder, "keyboard-check")
        swift.write_text(text)
        subprocess.run([
            "xcrun", "swiftc", "-O", "-swift-version", "5", "-target",
            f"{platform.machine()}-apple-macosx13.0", "-warnings-as-errors",
            str(swift), "-o", str(binary),
        ], check=True, timeout=60)
        subprocess.run([str(binary)], check=True, timeout=15)


if __name__ == "__main__":
    main()
