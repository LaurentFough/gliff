#!/usr/bin/env python3
"""Check actual picker Tab/Return/Escape handling without SSH or LAN access."""
import json
import platform
import struct
import subprocess
import sys
import tempfile
from pathlib import Path


def main():
    if sys.platform != "darwin":
        sys.exit("Run this check on macOS.")
    source = Path(__file__).resolve().parents[1] / "crates/gliff-macos/native/Gliff.swift"
    text = source.read_text().split("let mode = ProcessInfo", 1)[0]
    # Keep the production controls and delegate; supply a local host fixture.
    browse = 'browser.searchForServices(ofType: "_gliff._tcp.", inDomain: "local.")'
    assert browse in text, "cannot locate Bonjour start"
    text = text.replace(browse, "// Bonjour disabled in this keyboard fixture")
    harness = '''
let app=NSApplication.shared
app.setActivationPolicy(.regular)
let picker=ComputerPicker();app.delegate=picker
Timer.scheduledTimer(withTimeInterval:0.4,repeats:false) { _ in
    picker.nearby.records["fixture"] = NearbyComputer(id:"fixture",name:"Keyboard fixture",host:"keyboard.example",port:22,user:"tester",platform:"omarchy")
    picker.update()
    picker.table.selectRowIndexes(IndexSet(integer:0),byExtendingSelection:false)
    picker.port.stringValue="2222"
    let window=picker.window!
    window.makeFirstResponder(picker.address)
    let tab=NSEvent.keyEvent(with:.keyDown,location:.zero,modifierFlags:[],timestamp:0,windowNumber:window.windowNumber,context:nil,characters:"\\t",charactersIgnoringModifiers:"\\t",isARepeat:false,keyCode:48)!
    window.sendEvent(tab)
    precondition(!picker.finished,"Tab connected to a host")
    precondition(picker.port.currentEditor() != nil,"Tab did not focus the port field")
    diagnostic("TAB PASS: port focused without connecting")
    let cancel=CommandLine.arguments[1]=="cancel"
    let key=NSEvent.keyEvent(with:.keyDown,location:.zero,modifierFlags:[],timestamp:0,windowNumber:window.windowNumber,context:nil,characters:cancel ? "\\u{1b}" : "\\r",charactersIgnoringModifiers:cancel ? "\\u{1b}" : "\\r",isARepeat:false,keyCode:cancel ? 53 : 36)!
    window.sendEvent(key)
    precondition(picker.finished,"Return/Escape did not complete the picker")
}
withExtendedLifetime(picker) { app.run() }
'''
    with tempfile.TemporaryDirectory(prefix="gliff-picker-check-") as folder:
        swift = Path(folder, "main.swift")
        binary = Path(folder, "picker")
        swift.write_text(text + harness)
        subprocess.run([
            "xcrun", "swiftc", "-O", "-swift-version", "5", "-target",
            f"{platform.machine()}-apple-macosx13.0", "-warnings-as-errors",
            str(swift), "-o", str(binary),
        ], check=True, timeout=45)
        for mode in ("connect", "cancel"):
            child = subprocess.Popen([str(binary), mode], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                code = child.wait(timeout=15)
                errors = child.stderr.read().decode()
                assert code == 0, errors
                packet = child.stdout.read()
                assert len(packet) >= 5 and packet[0] == 2, "missing picker result"
                length = struct.unpack("<I", packet[1:5])[0]
                assert len(packet) == 5 + length, "unexpected extra picker result"
                event = json.loads(packet[5:])
                assert event["type"] == mode, event
                if mode == "connect":
                    assert event["host"] == "tester@keyboard.example" and event["port"] == 2222, event
                print(f"PICKER KEYBOARD PASS: Tab focuses Port without connecting; {mode}: {event}")
            finally:
                if child.poll() is None:
                    child.kill(); child.wait()


if __name__ == "__main__":
    main()
