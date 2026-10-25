#!/usr/bin/env python3
"""Check picker controls and default preferences inside and outside an app bundle."""
import json
import platform
import plistlib
import shutil
import struct
import subprocess
import sys
import tempfile
import uuid
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
let preferences=UserDefaults(suiteName:CommandLine.arguments[2])!
let checkDefault=CommandLine.arguments[1].hasPrefix("default-")
let picker=checkDefault ? ComputerPicker() : ComputerPicker(preferences:preferences);app.delegate=picker
Timer.scheduledTimer(withTimeInterval:0.4,repeats:false) { _ in
    let window=picker.window!
    if checkDefault {
        precondition(window.isVisible,"Default picker did not open")
        if CommandLine.arguments[1]=="default-bundled" {
            precondition(Bundle.main.bundleIdentifier=="org.omacom.gliff","Missing app-bundle fixture")
            precondition(picker.preferences===UserDefaults.standard,"Bundled picker must use standard preferences")
        }
        diagnostic("DEFAULT PREFERENCES PASS: picker opens and cancels")
        picker.cancel(nil)
        return
    }
    let emptySize=window.contentLayoutRect.size
    picker.nearby.records["fixture"] = NearbyComputer(id:"fixture",name:"Keyboard fixture",host:"keyboard.example",port:22,user:"tester",platform:"omarchy")
    picker.update()
    precondition(window.contentLayoutRect.height <= emptySize.height,"A single host added empty list space")
    for index in 1...9 {
        picker.nearby.records["extra-\\(index)"] = NearbyComputer(id:"extra-\\(index)",name:"Extra \\(index)",host:"extra-\\(index).example",port:22,user:"tester",platform:"omarchy")
    }
    picker.update()
    window.contentView!.layoutSubtreeIfNeeded()
    precondition(window.contentLayoutRect.height > emptySize.height,"Picker did not grow for multiple hosts")
    let scroll=picker.table.enclosingScrollView!
    precondition(scroll.documentVisibleRect.height < picker.table.bounds.height,"Long host list did not scroll")
    let enlarged=NSSize(width:window.contentLayoutRect.width+80,height:window.contentLayoutRect.height+80)
    window.setContentSize(enlarged)
    picker.nearby.records=picker.nearby.records.filter { $0.key=="fixture" }
    picker.update()
    precondition(window.contentLayoutRect.size==enlarged,"Discovery changed the user's window size")
    picker.nearby.failure="Nearby computers unavailable. Allow Gliff in System Settings → Privacy & Security → Local Network, or enter an SSH address."
    picker.update()
    window.setContentSize(window.contentMinSize)
    window.contentView!.layoutSubtreeIfNeeded()
    precondition(picker.status.bounds.height >= picker.status.cell!.cellSize(forBounds:picker.status.bounds).height,"Permission instructions were clipped")
    picker.nearby.failure=nil
    picker.update()
    window.contentView!.layoutSubtreeIfNeeded()
    for field in [picker.address,picker.port] {
        let rect=window.contentView!.convert(field.bounds,from:field)
        precondition(window.contentView!.bounds.contains(rect),"Compact picker clipped an SSH field")
    }
    diagnostic("LAYOUT PASS: compact list, scrolling, wrapped status and user resize")
    picker.table.selectRowIndexes(IndexSet(integer:0),byExtendingSelection:false)
    precondition(picker.screenMode=="mirror","New targets must keep the existing mirror default")
    picker.screen.selectItem(at:1);picker.screenChanged(nil)
    picker.address.stringValue="other.example";picker.restoreScreenPreference()
    precondition(picker.screenMode=="mirror","Private mode leaked to a different host")
    picker.address.stringValue="tester@keyboard.example";picker.restoreScreenPreference()
    precondition(picker.screenMode=="private","Private mode was not remembered for the host")
    picker.port.stringValue="2222"
    picker.restoreScreenPreference()
    precondition(picker.screenMode=="mirror","Screen preference leaked to a different SSH port")
    picker.screen.selectItem(at:1);picker.screenChanged(nil)
    precondition(!picker.screenDetail.stringValue.isEmpty)
    diagnostic("SCREEN PASS: mirror default, private selection and independent per-host/port preferences")
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
        bundle = Path(folder, "Gliff.app")
        bundled_binary = bundle / "Contents/MacOS/picker"
        bundled_binary.parent.mkdir(parents=True)
        shutil.copy2(binary, bundled_binary)
        with (bundle / "Contents/Info.plist").open("wb") as info:
            plistlib.dump({
                "CFBundleIdentifier": "org.omacom.gliff",
                "CFBundleExecutable": "picker",
                "CFBundlePackageType": "APPL",
                "CFBundleName": "Gliff",
            }, info)
        subprocess.run(["codesign", "--force", "--sign", "-", str(bundle)],
                       check=True, timeout=15)
        for mode in ("connect", "cancel", "default-standalone", "default-bundled"):
            suite = f"org.omacom.gliff.picker-check.{uuid.uuid4()}"
            executable = bundled_binary if mode == "default-bundled" else binary
            child = subprocess.Popen([str(executable), mode, suite], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                code = child.wait(timeout=15)
                errors = child.stderr.read().decode()
                assert code == 0, f"picker exited {code}: {errors}"
                packet = child.stdout.read()
                assert len(packet) >= 5 and packet[0] == 2, "missing picker result"
                length = struct.unpack("<I", packet[1:5])[0]
                assert len(packet) == 5 + length, "unexpected extra picker result"
                event = json.loads(packet[5:])
                expected = "cancel" if mode.startswith("default-") else mode
                assert event["type"] == expected, event
                if mode == "connect":
                    assert event["host"] == "tester@keyboard.example" and event["port"] == 2222, event
                    assert event["screen"] == "private", event
                if mode.startswith("default-"):
                    print(f"PICKER DEFAULT PREFERENCES PASS: {mode}; opens and cancels")
                else:
                    print(f"PICKER LAYOUT/KEYBOARD PASS: compact list, scrolling, wrapped status, user resize; Tab focuses Port without connecting; {mode}: {event}")
            finally:
                if child.poll() is None:
                    child.kill(); child.wait()
                subprocess.run(["defaults", "delete", suite], stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL, check=False)


if __name__ == "__main__":
    main()
