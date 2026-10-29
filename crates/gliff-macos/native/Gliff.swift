import AppKit
import ScreenCaptureKit
import CoreMedia
import CoreVideo
import ApplicationServices
import VideoToolbox
import Carbon
import Metal
import QuartzCore

// stdout belongs exclusively to the bridge. Diagnostics go to stderr.
let outputLock = NSLock()
func packet(_ kind: UInt8, _ data: Data) {
    outputLock.lock()
    defer { outputLock.unlock() }
    var header = Data([kind])
    var count = UInt32(data.count).littleEndian
    withUnsafeBytes(of: &count) { header.append(contentsOf: $0) }
    do {
        try FileHandle.standardOutput.write(contentsOf: header)
        try FileHandle.standardOutput.write(contentsOf: data)
    } catch { exit(0) }
}
func event(_ value: [String: Any]) {
    if let data = try? JSONSerialization.data(withJSONObject: value) { packet(2, data) }
}
func diagnostic(_ text: String) {
    FileHandle.standardError.write(Data((text + "\n").utf8))
}
func fail(_ text: String) -> Never {
    event(["type": "error", "message": text]); diagnostic(text); exit(1)
}
func exact(_ count: Int) -> Data? {
    // Pipe reads can return only a few KiB even for a Retina-size frame.
    // Requesting the entire remainder churns large allocations that malloc
    // caches after each short read; reserve once and keep read buffers small.
    var data = Data(capacity: count)
    while data.count < count {
        // Drain Foundation's temporary NSData after each short read instead of
        // keeping a frame's worth of chunk buffers until the packet completes.
        let received = autoreleasepool { () -> Bool in
            guard let part = try? FileHandle.standardInput.read(upToCount: min(count - data.count, 64 * 1024)), !part.isEmpty else { return false }
            data.append(part); return true
        }
        if !received { return nil }
    }
    return data
}
func number(_ data: Data, _ at: Int) -> UInt32 {
    data[at..<at+4].enumerated().reduce(UInt32(0)) { $0 | UInt32($1.element) << (8 * $1.offset) }
}
func append(_ number: UInt32, _ to: inout Data) {
    var n = number.littleEndian
    withUnsafeBytes(of: &n) { to.append(contentsOf: $0) }
}
func readCommands(onClose: @escaping () -> Void = {}, _ callback: @escaping (UInt8, Data) -> Void) {
    DispatchQueue.global(qos: .userInteractive).async {
        // This dispatch work item lives for the entire connection. Foundation
        // pipe reads autorelease NSData, so drain each packet, not just on exit.
        while autoreleasepool(invoking: { () -> Bool in
            guard let header = exact(5) else { return false }
            let count = Int(number(header, 1))
            guard count <= 64 * 1024 * 1024, let data = exact(count) else { return false }
            callback(header[0], data)
            return true
        }) {}
        DispatchQueue.main.async { onClose(); NSApplication.shared.terminate(nil); exit(0) }
    }
}
func json(_ data: Data) -> [String: Any] {
    (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
}

// Read a Unicode layout without selecting it or changing the user's input source.
// Keep the actual Mac symbols, including Option levels and dead keys, rather
// than guessing an XKB layout from the input source's display name.
func keyboardLayout(_ identifier: String? = nil) -> [String: Any]? {
    let source: TISInputSource
    if let identifier = identifier {
        guard let list = TISCreateInputSourceList([kTISPropertyInputSourceID as String: identifier] as CFDictionary, true)?.takeRetainedValue() else { return nil }
        let matches = list as! [TISInputSource]
        guard let first = matches.first else { return nil }
        source = first
    } else {
        guard let current = TISCopyCurrentKeyboardLayoutInputSource()?.takeRetainedValue() else { return nil }
        source = current
    }
    guard let raw = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData) else { return nil }
    let data = Unmanaged<CFData>.fromOpaque(raw).takeUnretainedValue()
    guard let bytes = CFDataGetBytePtr(data) else { return nil }
    let layout = UnsafeRawPointer(bytes).assumingMemoryBound(to: UCKeyboardLayout.self)
    var keys: [[String: Any]] = []
    for code in UInt16(0)..<128 {
        var levels: [[String: Any]] = []
        for level in 0..<8 {
            let flags = (level & 1 == 0 ? 0 : shiftKey) | (level & 2 == 0 ? 0 : optionKey) | (level & 4 == 0 ? 0 : alphaLock)
            var state: UInt32 = 0
            var length = 0
            var chars = [UniChar](repeating: 0, count: 16)
            var status = UCKeyTranslate(layout, code, UInt16(kUCKeyActionDown), UInt32(flags >> 8), UInt32(LMGetKbdType()), 0, &state, chars.count, &length, &chars)
            let dead = state != 0
            if dead {
                state = 0
                status = UCKeyTranslate(layout, code, UInt16(kUCKeyActionDisplay), UInt32(flags >> 8), UInt32(LMGetKbdType()), OptionBits(kUCKeyTranslateNoDeadKeysMask), &state, chars.count, &length, &chars)
            }
            levels.append(["text": status == noErr && length <= chars.count ? String(utf16CodeUnits: chars, count: length) : "", "dead": dead])
        }
        keys.append(["keycode": Int(code), "levels": levels])
    }
    let name = TISGetInputSourceProperty(source, kTISPropertyInputSourceID).map { Unmanaged<CFString>.fromOpaque($0).takeUnretainedValue() as String } ?? "unknown"
    return ["type": "keyboard_layout", "source": name, "keys": keys]
}

// NSPasteboard requests data only when an application pastes. The pipe reader
// resolves the wait directly, so a paste callback on the main thread can wait.
final class PasteProvider: NSObject, NSPasteboardItemDataProvider {
    let serial: UInt32
    let condition = NSCondition()
    var text: String?
    var finished = false
    var requested = false
    init(_ serial: UInt32) { self.serial = serial }
    func pasteboard(_ pasteboard: NSPasteboard?, item: NSPasteboardItem, provideDataForType type: NSPasteboard.PasteboardType) {
        condition.lock()
        if !requested { requested = true; event(["type": "paste", "serial": serial]) }
        let deadline = Date(timeIntervalSinceNow: 30)
        while !finished { if !condition.wait(until: deadline) { break } }
        let value = text
        condition.unlock()
        if let value = value { item.setString(value, forType: type) }
    }
    func receive(_ cmd: [String: Any]) {
        condition.lock(); text = cmd["text"] as? String; finished = true
        condition.broadcast(); condition.unlock()
    }
}
final class Clipboard {
    var provider: PasteProvider?
    var observed = Int.min
    var timer: Timer?
    let lock = NSLock()
    func start() {
        timer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in self?.poll() }
    }
    func poll() {
        let pb = NSPasteboard.general
        guard pb.changeCount != observed else { return }
        observed = pb.changeCount
        lock.lock(); provider = nil; lock.unlock()
        let text = pb.string(forType: .string)
        event(["type": "clipboard", "text": (text.map { $0.utf8.count <= 1024*1024 ? $0 : "" } as Any?) ?? NSNull()])
    }
    // Called on the pipe reader, including while the main thread is pasting.
    func receive(_ cmd: [String: Any]) -> Bool {
        guard cmd["type"] as? String == "clipboard_data" else { return false }
        lock.lock(); let p = provider; lock.unlock()
        if cmd["serial"] as? UInt32 == p?.serial { p?.receive(cmd) }
        return true
    }
    func command(_ cmd: [String: Any]) {
        guard cmd["type"] as? String == "clipboard_offer" else { return }
        let pb = NSPasteboard.general
        pb.clearContents()
        lock.lock()
        if cmd["text"] as? Bool == true, let serial = cmd["serial"] as? UInt32 {
            let p = PasteProvider(serial); let item = NSPasteboardItem()
            item.setDataProvider(p, forTypes: [.string]); provider = p
            pb.writeObjects([item])
        } else { provider = nil }
        lock.unlock()
        observed = pb.changeCount
    }
}
let clipboard = Clipboard()

final class Input {
    var display: CGDirectDisplayID = CGMainDisplayID()
    var keys: Set<CGKeyCode> = []
    var buttons: Set<Int> = []
    var position: CGPoint = .zero
    var capsLock = CGEventSource.flagsState(.combinedSessionState).contains(.maskAlphaShift)
    let source = CGEventSource(stateID: .privateState)
    func flags() -> CGEventFlags {
        var f: CGEventFlags = []
        if capsLock { f.insert(.maskAlphaShift) }
        if keys.contains(55) || keys.contains(54) { f.insert(.maskCommand) }
        if keys.contains(56) || keys.contains(60) { f.insert(.maskShift) }
        if keys.contains(58) || keys.contains(61) { f.insert(.maskAlternate) }
        if keys.contains(59) || keys.contains(62) { f.insert(.maskControl) }
        return f
    }
    func handle(_ cmd: [String: Any]) {
        switch cmd["type"] as? String {
        case "key":
            guard let code = cmd["keycode"] as? UInt16, let down = cmd["pressed"] as? Bool else { return }
            if code == 57 && down && !keys.contains(code) { capsLock.toggle() }
            if down { keys.insert(code) } else { keys.remove(code) }
            let e = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down)
            e?.flags = flags(); e?.post(tap: .cghidEventTap)
        case "motion":
            let bounds = CGDisplayBounds(display)
            let x = min(max(cmd["x"] as? Double ?? 0, 0), max(0, bounds.width - 1))
            let y = min(max(cmd["y"] as? Double ?? 0, 0), max(0, bounds.height - 1))
            position = CGPoint(x: bounds.minX + x, y: bounds.minY + y)
            let type: CGEventType = buttons.contains(0) ? .leftMouseDragged : buttons.contains(1) ? .rightMouseDragged : buttons.isEmpty ? .mouseMoved : .otherMouseDragged
            let e = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: position, mouseButton: buttons.contains(1) ? .right : .left)
            e?.flags = flags(); e?.post(tap: .cghidEventTap)
        case "button":
            guard let button = cmd["button"] as? Int, let down = cmd["pressed"] as? Bool, let b = CGMouseButton(rawValue: UInt32(button)) else { return }
            if down { buttons.insert(button) } else { buttons.remove(button) }
            let type: CGEventType = button == 0 ? (down ? .leftMouseDown : .leftMouseUp) : button == 1 ? (down ? .rightMouseDown : .rightMouseUp) : (down ? .otherMouseDown : .otherMouseUp)
            let e = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: position, mouseButton: b)
            e?.flags = flags(); e?.post(tap: .cghidEventTap)
        case "axis":
            let value = Int32(max(-32768, min(32767, cmd["value"] as? Double ?? 0)))
            let horizontal = cmd["horizontal"] as? Bool ?? false
            let e = CGEvent(scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2, wheel1: horizontal ? 0 : -value, wheel2: horizontal ? -value : 0, wheel3: 0)
            e?.flags = flags(); e?.post(tap: .cghidEventTap)
        case "release":
            for key in Array(keys) { handle(["type": "key", "keycode": key, "pressed": false]) }
            for button in Array(buttons) { handle(["type": "button", "button": button, "pressed": false]) }
        default: break
        }
    }
}

// Each request completes before accepting another frame. This bounds retained
// VideoToolbox surfaces even if the SSH client stops acknowledging video.
final class HardwareEncoder {
    var session: VTCompressionSession?
    let width: Int, height: Int, fps: Int
    var sequence: Int64 = 0
    var produced = false
    init(_ width: Int, _ height: Int, _ bitrate: Int, _ fps: Int) throws {
        self.width = width; self.height = height; self.fps = fps
        guard width >= 2, width <= 1920, height >= 2, height <= 1200, width % 2 == 0, height % 2 == 0, fps > 0 else { throw NSError(domain: "Gliff.Encoder", code: 1) }
        let specification = [kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true] as CFDictionary
        let attributes = [kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
                          kCVPixelBufferWidthKey: width, kCVPixelBufferHeightKey: height,
                          kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary
        let callback: VTCompressionOutputCallback = { refcon, _, status, flags, sample in
            guard let refcon = refcon else { return }
            let encoder = Unmanaged<HardwareEncoder>.fromOpaque(refcon).takeUnretainedValue()
            autoreleasepool { encoder.output(status, flags, sample) }
        }
        do {
            try check(VTCompressionSessionCreate(allocator: kCFAllocatorDefault, width: Int32(width), height: Int32(height), codecType: kCMVideoCodecType_H264,
                encoderSpecification: specification, imageBufferAttributes: attributes, compressedDataAllocator: nil,
                outputCallback: callback, refcon: Unmanaged.passUnretained(self).toOpaque(), compressionSessionOut: &session), "create hardware encoder")
            guard let session = session else { throw NSError(domain: "Gliff.Encoder", code: 2) }
            for (key, value) in [(kVTCompressionPropertyKey_RealTime, true as CFBoolean),
                                 (kVTCompressionPropertyKey_AllowFrameReordering, false as CFBoolean)] {
                try check(VTSessionSetProperty(session, key: key, value: value), "set \(key)")
            }
            try check(VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ProfileLevel, value: kVTProfileLevel_H264_Baseline_AutoLevel), "set H.264 profile")
            try check(VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AverageBitRate, value: bitrate as CFNumber), "set bitrate")
            try check(VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ExpectedFrameRate, value: fps as CFNumber), "set frame rate")
            try check(VTSessionSetProperty(session, key: kVTCompressionPropertyKey_MaxKeyFrameInterval, value: (fps * 2) as CFNumber), "set keyframe interval")
            try check(VTCompressionSessionPrepareToEncodeFrames(session), "prepare hardware encoder")
            var hardware: CFTypeRef?
            let hardwareStatus = withUnsafeMutablePointer(to: &hardware) { pointer in
                VTSessionCopyProperty(session, key: kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, allocator: nil, valueOut: UnsafeMutableRawPointer(pointer))
            }
            try check(hardwareStatus, "check hardware encoder")
            guard (hardware as? Bool) == true else { throw NSError(domain: "Gliff.Encoder", code: 3, userInfo: [NSLocalizedDescriptionKey: "VideoToolbox selected a software encoder"]) }
        } catch { close(); throw error }
    }
    func check(_ status: OSStatus, _ operation: String) throws {
        if status != noErr { throw NSError(domain: "Gliff.Encoder", code: Int(status), userInfo: [NSLocalizedDescriptionKey: "\(operation) failed (\(status))"]) }
    }
    func close() {
        if let session = session { VTCompressionSessionInvalidate(session); self.session = nil }
    }
    deinit { close() }
    func encode(_ data: Data) {
        guard data.count == 16 + width * height * 4, number(data, 0) == width, number(data, 4) == height, number(data, 12) <= 1,
              let session = session, let pool = VTCompressionSessionGetPixelBufferPool(session) else { fail("Invalid hardware encoder frame") }
        do {
            var pixel: CVPixelBuffer?
            let limits = [kCVPixelBufferPoolAllocationThresholdKey: 3] as CFDictionary
            try check(CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(kCFAllocatorDefault, pool, limits, &pixel), "allocate bounded encoder surface")
            guard let pixel = pixel else { fail("Could not allocate encoder surface") }
            try check(CVPixelBufferLockBaseAddress(pixel, []), "lock encoder surface")
            let base = CVPixelBufferGetBaseAddress(pixel)!
            let stride = CVPixelBufferGetBytesPerRow(pixel)
            data.withUnsafeBytes { raw in
                for row in 0..<height { base.advanced(by: row * stride).copyMemory(from: raw.baseAddress!.advanced(by: 16 + row * width * 4), byteCount: width * 4) }
            }
            CVPixelBufferUnlockBaseAddress(pixel, [])
            let properties = [kVTEncodeFrameOptionKey_ForceKeyFrame: number(data, 12) == 1] as CFDictionary
            produced = false
            try check(VTCompressionSessionEncodeFrame(session, imageBuffer: pixel, presentationTimeStamp: CMTime(value: sequence, timescale: Int32(fps)),
                duration: CMTime(value: 1, timescale: Int32(fps)), frameProperties: properties, sourceFrameRefcon: nil, infoFlagsOut: nil), "encode hardware frame")
            sequence += 1
            try check(VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid), "complete hardware frame")
            if !produced { fail("Hardware encoder did not produce a frame") }
        } catch { fail(error.localizedDescription) }
    }
    func output(_ status: OSStatus, _ flags: VTEncodeInfoFlags, _ sample: CMSampleBuffer?) {
        guard status == noErr, !flags.contains(.frameDropped), let sample = sample, sample.isValid,
              let format = sample.formatDescription, let buffer = sample.dataBuffer else { fail("Hardware encoder output failed (\(status))") }
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[CFString: Any]]
        let keyframe = (attachments?.first?[kCMSampleAttachmentKey_NotSync] as? Bool) != true
        var encoded = Data()
        append(keyframe ? 1 : 0, &encoded)
        var pointer: UnsafePointer<UInt8>?, size = 0, count = 0, headerLength: Int32 = 0
        guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, parameterSetIndex: 0, parameterSetPointerOut: &pointer, parameterSetSizeOut: &size, parameterSetCountOut: &count, nalUnitHeaderLengthOut: &headerLength) == noErr,
              headerLength >= 1, headerLength <= 4 else { fail("Invalid H.264 parameter sets") }
        if keyframe {
            for i in 0..<count {
                guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, parameterSetIndex: i, parameterSetPointerOut: &pointer, parameterSetSizeOut: &size, parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil) == noErr,
                      let pointer = pointer, size > 0 else { fail("Invalid H.264 parameter set") }
                encoded.append(contentsOf: [0, 0, 0, 1]); encoded.append(pointer, count: size)
            }
        }
        let length = CMBlockBufferGetDataLength(buffer)
        var avcc = Data(count: length)
        let copied = avcc.withUnsafeMutableBytes { raw in CMBlockBufferCopyDataBytes(buffer, atOffset: 0, dataLength: length, destination: raw.baseAddress!) }
        guard copied == noErr else { fail("Could not read H.264 output") }
        var offset = 0
        while offset < avcc.count {
            guard offset + Int(headerLength) <= avcc.count else { fail("Truncated H.264 length") }
            var size = 0
            for byte in avcc[offset..<offset + Int(headerLength)] { size = (size << 8) | Int(byte) }
            offset += Int(headerLength)
            guard size > 0, size <= avcc.count - offset else { fail("Truncated H.264 NAL") }
            encoded.append(contentsOf: [0, 0, 0, 1]); encoded.append(avcc[offset..<offset + size]); offset += size
        }
        produced = true
        packet(3, encoded)
    }
}

final class Capture: NSObject, SCStreamOutput, SCStreamDelegate {
    var stream: SCStream?
    let input = Input()
    var scale: UInt32 = 1000
    var config: SCStreamConfiguration?
    func resize(_ width: Int, _ height: Int) {
        guard let stream = stream, let config = config else { return }
        let pw = CGDisplayPixelsWide(input.display), ph = CGDisplayPixelsHigh(input.display)
        let ratio = min(1, min(Double(max(2, min(width, 1920))) / Double(pw), Double(max(2, min(height, 1200))) / Double(ph)))
        let w = max(2, Int(Double(pw) * ratio) / 2 * 2), h = max(2, Int(Double(ph) * ratio) / 2 * 2)
        if config.width == w && config.height == h { return }
        config.width = w; config.height = h
        Task { do { try await stream.updateConfiguration(config) } catch { fail("Could not resize capture: \(error.localizedDescription)") } }
    }
    func start(_ requested: String, _ maxWidth: Int, _ maxHeight: Int, _ fps: Int) async {
        guard CGPreflightScreenCaptureAccess() else {
            _ = CGRequestScreenCaptureAccess()
            fail("Screen Recording permission is required. Open Gliff on this Mac and enable it in System Settings > Privacy & Security > Screen & System Audio Recording, then reconnect.")
        }
        guard AXIsProcessTrusted() else {
            fail("Accessibility permission is required to control this Mac. Enable Gliff in System Settings > Privacy & Security > Accessibility, then reconnect.")
        }
        do {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
            guard let display = content.displays.first(where: { requested == "auto" ? $0.displayID == CGMainDisplayID() : String($0.displayID) == requested || "display-\($0.displayID)" == requested }) else {
                fail("Display \(requested) was not found. Use gliff-server --outputs to list displays.")
            }
            input.display = display.displayID
            let physicalWidth = CGDisplayPixelsWide(display.displayID)
            let physicalHeight = CGDisplayPixelsHigh(display.displayID)
            let ratio = min(1, min(Double(maxWidth) / Double(physicalWidth), Double(maxHeight) / Double(physicalHeight)))
            let width = max(2, Int(Double(physicalWidth) * ratio) / 2 * 2)
            let height = max(2, Int(Double(physicalHeight) * ratio) / 2 * 2)
            scale = UInt32(max(1, Double(width) / CGDisplayBounds(display.displayID).width * 1000))
            let config = SCStreamConfiguration()
            config.width = width; config.height = height
            config.pixelFormat = kCVPixelFormatType_32BGRA
            config.colorSpaceName = CGColorSpace.sRGB
            config.minimumFrameInterval = CMTime(value: 1, timescale: Int32(fps))
            config.queueDepth = 3; config.showsCursor = true
            self.config = config
            let filter = SCContentFilter(display: display, excludingWindows: [])
            let stream = SCStream(filter: filter, configuration: config, delegate: self)
            try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: DispatchQueue(label: "gliff.capture", qos: .userInteractive))
            self.stream = stream
            event(["type": "ready", "width": width, "height": height, "scale_milli": scale, "output": "display-\(display.displayID)"])
            try await stream.startCapture()
            DispatchQueue.main.async { clipboard.start() }
        } catch { fail("Screen capture failed: \(error.localizedDescription)") }
    }
    func stream(_ stream: SCStream, didStopWithError error: Error) { fail("Screen capture stopped: \(error.localizedDescription)") }
    func stream(_ stream: SCStream, didOutputSampleBuffer sample: CMSampleBuffer, of type: SCStreamOutputType) {
        autoreleasepool { sendFrame(sample, type) }
    }
    func sendFrame(_ sample: CMSampleBuffer, _ type: SCStreamOutputType) {
        guard type == .screen, sample.isValid, let buffer = sample.imageBuffer else { return }
        // Skip idle/blank samples; ScreenCaptureKit can reuse their previous image.
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]], let raw = attachments.first?[.status] as? Int, raw != SCFrameStatus.complete.rawValue { return }
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(buffer) else { return }
        let width = CVPixelBufferGetWidth(buffer), height = CVPixelBufferGetHeight(buffer)
        let stride = CVPixelBufferGetBytesPerRow(buffer)
        var data = Data(capacity: 12 + width * height * 4)
        let frameScale = UInt32(max(1, Double(width) / CGDisplayBounds(input.display).width * 1000))
        append(UInt32(width), &data); append(UInt32(height), &data); append(frameScale, &data)
        for row in 0..<height { data.append(base.advanced(by: row * stride).assumingMemoryBound(to: UInt8.self), count: width * 4) }
        packet(1, data)
    }
}

// MARK: - GPU video
//
// Hardware decode for the viewer: VideoToolbox decodes each H.264 stream into
// IOSurface-backed NV12 buffers, a Metal kernel recombines AVC444 (Dual420)
// or spreads 4:2:0 chroma (Single420) into a BGRA texture, and a CAMetalLayer
// shows it. No pixel touches the CPU; the client only forwards coded bytes.
//
// Ported from omacom/gliff#5 (macos/Sources/GliffVideo, David Heinemeier
// Hansson), MIT. Changes: one file, macOS 13 (plain NSLock instead of
// withLock), GPU-prefixed names, and a viewer-side GPUVideo/GPUPresenter.

enum NalType: UInt8 {
    case slice = 1
    case idr = 5
    case sei = 6
    case sps = 7
    case pps = 8
    case accessUnitDelimiter = 9
}

/// One NAL unit, from its header byte on (no start code).
struct Nal {
    let data: Data
    var type: UInt8 { data[data.startIndex] & 0x1f }
}

/// Split an Annex B stream at its start codes. Trailing zero bytes belong to
/// the next 4-byte start code or to trailing_zero_8bits, never to a NAL.
func nalUnits(_ data: Data) -> [Nal] {
    let bytes = [UInt8](data)
    var starts: [Int] = []
    var i = 0
    while i + 3 <= bytes.count {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            starts.append(i + 3)
            i += 3
        } else {
            i += 1
        }
    }
    var units: [Nal] = []
    for (n, body) in starts.enumerated() {
        var end = n + 1 < starts.count ? starts[n + 1] - 3 : bytes.count
        while end > body && bytes[end - 1] == 0 { end -= 1 }
        if end > body { units.append(Nal(data: Data(bytes[body..<end]))) }
    }
    return units
}

/// An access unit as VideoToolbox wants it: parameter sets apart, and the
/// other NAL units length-prefixed (AVCC, 4-byte big-endian lengths).
struct AccessUnit {
    var sps: Data?
    var pps: Data?
    var avcc = Data()

    init(annexB: Data) {
        for nal in nalUnits(annexB) {
            switch nal.type {
            case NalType.sps.rawValue: sps = nal.data
            case NalType.pps.rawValue: pps = nal.data
            case NalType.accessUnitDelimiter.rawValue: break
            default:
                var length = UInt32(nal.data.count).bigEndian
                avcc.append(Data(bytes: &length, count: 4))
                avcc.append(nal.data)
            }
        }
    }
}

enum GPUVideoError: Error, CustomStringConvertible {
    case noParameterSets
    case status(String, OSStatus)
    case noDevice
    case texture(String)
    case sizeMismatch
    case malformed(String)

    var description: String {
        switch self {
        case .noParameterSets: return "no SPS/PPS before the first slice"
        case .status(let what, let code): return "\(what) failed: OSStatus \(code)"
        case .noDevice: return "no Metal device"
        case .texture(let what): return "cannot wrap \(what) as a Metal texture"
        case .sizeMismatch: return "main and auxiliary pictures differ in size"
        case .malformed(let what): return "malformed video packet: \(what)"
        }
    }
}

/// One H.264 stream. Decoding is synchronous (no asynchronous flag), and
/// gliff's streams never reorder, so each access unit gives at most one
/// picture before DecodeFrame returns.
final class H264Decoder {
    private var format: CMVideoFormatDescription?
    private var session: VTDecompressionSession?
    private var sps: Data?
    private var pps: Data?

    deinit {
        if let session = session { VTDecompressionSessionInvalidate(session) }
    }

    /// Decode one Annex B access unit; nil when it produced no picture.
    func decode(_ annexB: Data) throws -> CVPixelBuffer? {
        let unit = AccessUnit(annexB: annexB)
        if let sps = unit.sps, let pps = unit.pps, sps != self.sps || pps != self.pps {
            try configure(sps: sps, pps: pps)
        }
        guard let session = session, let format = format else { throw GPUVideoError.noParameterSets }
        if unit.avcc.isEmpty { return nil }
        let sample = try sampleBuffer(unit.avcc, format: format)
        var image: CVPixelBuffer?
        var decodeStatus = noErr
        let status = VTDecompressionSessionDecodeFrame(session, sampleBuffer: sample, flags: [], infoFlagsOut: nil) { status, _, imageBuffer, _, _ in
            decodeStatus = status
            image = imageBuffer
        }
        guard status == noErr else { throw GPUVideoError.status("DecodeFrame", status) }
        guard decodeStatus == noErr else { throw GPUVideoError.status("decode", decodeStatus) }
        return image
    }

    /// (Re)create the session for new parameter sets; the size comes from
    /// the SPS, cropping included.
    private func configure(sps: Data, pps: Data) throws {
        var newFormat: CMVideoFormatDescription?
        let status = sps.withUnsafeBytes { spsBytes in
            pps.withUnsafeBytes { ppsBytes -> OSStatus in
                let pointers = [
                    spsBytes.bindMemory(to: UInt8.self).baseAddress!,
                    ppsBytes.bindMemory(to: UInt8.self).baseAddress!,
                ]
                let sizes = [sps.count, pps.count]
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault, parameterSetCount: 2,
                    parameterSetPointers: pointers, parameterSetSizes: sizes,
                    nalUnitHeaderLength: 4, formatDescriptionOut: &newFormat)
            }
        }
        guard status == noErr, let newFormat = newFormat else { throw GPUVideoError.status("format description", status) }
        if let session = session, VTDecompressionSessionCanAcceptFormatDescription(session, formatDescription: newFormat) {
            format = newFormat
        } else {
            if let session = session { VTDecompressionSessionInvalidate(session); self.session = nil }
            let spec: [CFString: Any] = [kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: true]
            let attributes: [CFString: Any] = [
                kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                kCVPixelBufferMetalCompatibilityKey: true,
                kCVPixelBufferIOSurfacePropertiesKey: [CFString: Any](),
            ]
            var newSession: VTDecompressionSession?
            let created = VTDecompressionSessionCreate(
                allocator: kCFAllocatorDefault, formatDescription: newFormat,
                decoderSpecification: spec as CFDictionary, imageBufferAttributes: attributes as CFDictionary,
                outputCallback: nil, decompressionSessionOut: &newSession)
            guard created == noErr, let newSession = newSession else { throw GPUVideoError.status("session create", created) }
            VTSessionSetProperty(newSession, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)
            session = newSession
            format = newFormat
        }
        self.sps = sps
        self.pps = pps
    }

    private func sampleBuffer(_ avcc: Data, format: CMVideoFormatDescription) throws -> CMSampleBuffer {
        var block: CMBlockBuffer?
        var status = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: nil, blockLength: avcc.count,
            blockAllocator: kCFAllocatorDefault, customBlockSource: nil, offsetToData: 0,
            dataLength: avcc.count, flags: kCMBlockBufferAssureMemoryNowFlag, blockBufferOut: &block)
        guard status == noErr, let block = block else { throw GPUVideoError.status("block buffer", status) }
        status = avcc.withUnsafeBytes { bytes in
            CMBlockBufferReplaceDataBytes(with: bytes.baseAddress!, blockBuffer: block, offsetIntoDestination: 0, dataLength: avcc.count)
        }
        guard status == noErr else { throw GPUVideoError.status("block copy", status) }
        var sample: CMSampleBuffer?
        var size = avcc.count
        status = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault, dataBuffer: block, formatDescription: format,
            sampleCount: 1, sampleTimingEntryCount: 0, sampleTimingArray: nil,
            sampleSizeEntryCount: 1, sampleSizeArray: &size, sampleBufferOut: &sample)
        guard status == noErr, let sample = sample else { throw GPUVideoError.status("sample buffer", status) }
        return sample
    }
}

/// AVC444 recombine on the GPU, a port of crates/gliff-vk/shaders/recombine.comp:
/// the main and auxiliary NV12 pictures become one full-chroma BGRA texture.
/// With no auxiliary picture the main chroma covers each 2x2 block (Single420).
final class Recombiner {
    let device: MTLDevice
    private let queue: MTLCommandQueue
    private let pipeline: MTLComputePipelineState
    private let cache: CVMetalTextureCache

    init(device: MTLDevice) throws {
        guard let queue = device.makeCommandQueue() else { throw GPUVideoError.noDevice }
        let options = MTLCompileOptions()
        // The constants must round exactly as the Vulkan shader's do.
        if #available(macOS 15.0, *) { options.mathMode = .safe } else { options.fastMathEnabled = false }
        let library = try device.makeLibrary(source: Recombiner.source, options: options)
        guard let function = library.makeFunction(name: "recombine") else { throw GPUVideoError.noDevice }
        self.device = device
        self.queue = queue
        self.pipeline = try device.makeComputePipelineState(function: function)
        var cache: CVMetalTextureCache?
        CVMetalTextureCacheCreate(kCFAllocatorDefault, nil, device, nil, &cache)
        guard let made = cache else { throw GPUVideoError.noDevice }
        self.cache = made
    }

    /// A texture the recombine writes and a renderer samples.
    func makeOutput(width: Int, height: Int) throws -> MTLTexture {
        let descriptor = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm, width: width, height: height, mipmapped: false)
        descriptor.usage = [.shaderWrite, .shaderRead]
        descriptor.storageMode = .private
        guard let output = device.makeTexture(descriptor: descriptor) else { throw GPUVideoError.texture("output") }
        return output
    }

    /// Recombine one frame into `output` and wait for the GPU, so the frame
    /// is complete when the client acknowledges it.
    func recombine(main: CVPixelBuffer, aux: CVPixelBuffer?, into output: MTLTexture) throws {
        let width = CVPixelBufferGetWidth(main), height = CVPixelBufferGetHeight(main)
        if let aux = aux, CVPixelBufferGetWidth(aux) != width || CVPixelBufferGetHeight(aux) != height { throw GPUVideoError.sizeMismatch }
        guard output.width == width, output.height == height else { throw GPUVideoError.sizeMismatch }
        let planes = [
            try plane(main, 0, .r8Unorm, "main luma"),
            try plane(main, 1, .rg8Unorm, "main chroma"),
            try plane(aux ?? main, 0, .r8Unorm, "aux luma"),
            try plane(aux ?? main, 1, .rg8Unorm, "aux chroma"),
        ]
        guard let commands = queue.makeCommandBuffer(), let encoder = commands.makeComputeCommandEncoder() else {
            throw GPUVideoError.texture("command buffer")
        }
        var params: [Int32] = [Int32(width), Int32(height), aux == nil ? 0 : 1]
        encoder.setComputePipelineState(pipeline)
        for (index, texture) in planes.enumerated() { encoder.setTexture(CVMetalTextureGetTexture(texture), index: index) }
        encoder.setTexture(output, index: 4)
        encoder.setBytes(&params, length: MemoryLayout<Int32>.stride * params.count, index: 0)
        // One thread per 2x2 block, as in the Vulkan shader.
        let grid = MTLSize(width: (width + 1) / 2, height: (height + 1) / 2, depth: 1)
        encoder.dispatchThreads(grid, threadsPerThreadgroup: MTLSize(width: 16, height: 16, depth: 1))
        encoder.endEncoding()
        commands.addCompletedHandler { _ in _ = planes }
        commands.commit()
        commands.waitUntilCompleted()
        if let error = commands.error { throw error }
    }

    private func plane(_ buffer: CVPixelBuffer, _ index: Int, _ format: MTLPixelFormat, _ what: String) throws -> CVMetalTexture {
        var texture: CVMetalTexture?
        let status = CVMetalTextureCacheCreateTextureFromImage(
            kCFAllocatorDefault, cache, buffer, nil, format,
            CVPixelBufferGetWidthOfPlane(buffer, index), CVPixelBufferGetHeightOfPlane(buffer, index), index, &texture)
        guard status == kCVReturnSuccess, let made = texture else { throw GPUVideoError.texture(what) }
        return made
    }

    // BT.709 limited range, matching gliff-proto::color. The output is
    // bgra8Unorm, so the kernel writes logical RGBA and Metal stores B,G,R,A.
    static let source = """
    #include <metal_stdlib>
    using namespace metal;

    struct Params { int width; int height; int has_aux; };

    static float3 to_rgb(float y8, float u8, float v8) {
        float y = y8 * 255.0 - 16.0;
        float cb = u8 * 255.0 - 128.0;
        float cr = v8 * 255.0 - 128.0;
        return clamp(float3(1.1644 * y + 1.7927 * cr,
                            1.1644 * y - 0.2132 * cb - 0.5329 * cr,
                            1.1644 * y + 2.1124 * cb) / 255.0, 0.0, 1.0);
    }

    kernel void recombine(texture2d<float, access::read> main_y [[texture(0)]],
                          texture2d<float, access::read> main_uv [[texture(1)]],
                          texture2d<float, access::read> aux_y [[texture(2)]],
                          texture2d<float, access::read> aux_uv [[texture(3)]],
                          texture2d<float, access::write> dst [[texture(4)]],
                          constant Params &p [[buffer(0)]],
                          uint2 gid [[thread_position_in_grid]]) {
        int bx = int(gid.x), by = int(gid.y);
        int x = 2 * bx, y = 2 * by;
        if (x >= p.width || y >= p.height) return;
        float y00 = main_y.read(uint2(x, y)).r;
        float y10 = main_y.read(uint2(x + 1, y)).r;
        float y01 = main_y.read(uint2(x, y + 1)).r;
        float y11 = main_y.read(uint2(x + 1, y + 1)).r;
        float2 c00 = main_uv.read(uint2(bx, by)).rg;
        float2 c10 = c00, c01 = c00, c11 = c00;
        if (p.has_aux != 0) {
            int half_h = p.height / 2;
            c10 = aux_uv.read(uint2(bx, by)).rg;
            c01 = float2(aux_y.read(uint2(x, by)).r, aux_y.read(uint2(x, half_h + by)).r);
            c11 = float2(aux_y.read(uint2(x + 1, by)).r, aux_y.read(uint2(x + 1, half_h + by)).r);
        }
        dst.write(float4(to_rgb(y00, c00.r, c00.g), 1.0), uint2(x, y));
        dst.write(float4(to_rgb(y10, c10.r, c10.g), 1.0), uint2(x + 1, y));
        dst.write(float4(to_rgb(y01, c01.r, c01.g), 1.0), uint2(x, y + 1));
        dst.write(float4(to_rgb(y11, c11.r, c11.g), 1.0), uint2(x + 1, y + 1));
    }
    """
}

/// Output textures reused across frames.
final class TexturePool {
    private let lock = NSLock()
    private var free: [MTLTexture]
    init(_ textures: [MTLTexture]) { free = textures }
    func take() -> MTLTexture? { lock.lock(); defer { lock.unlock() }; return free.popLast() }
    func give(back texture: MTLTexture) { lock.lock(); free.append(texture); lock.unlock() }
}

/// A decoded frame; its texture returns to the pool when the last reference
/// goes, so the display keeps it exactly as long as the GPU still reads it.
final class GPUFrame {
    let texture: MTLTexture
    private let pool: TexturePool
    init(texture: MTLTexture, pool: TexturePool) { self.texture = texture; self.pool = pool }
    deinit { pool.give(back: texture) }
}

private final class DecodeResults: @unchecked Sendable {
    private let lock = NSLock()
    private(set) var pictures: [CVPixelBuffer?] = [nil, nil]
    private(set) var failure: Error?
    func set(_ stream: Int, _ picture: CVPixelBuffer?) { lock.lock(); pictures[stream] = picture; lock.unlock() }
    func fail(_ error: Error) { lock.lock(); failure = error; lock.unlock() }
}

/// One stream's client-side video: two decoders (one for Single420) and the
/// recombine, into a small pool of output textures.
final class FramePipeline {
    let width: Int
    let height: Int
    let dual: Bool
    private let recombiner: Recombiner
    private let main = H264Decoder()
    private let aux: H264Decoder?
    private let pool: TexturePool
    /// On screen, waiting to be drawn, being decoded, and a spare.
    static let poolSize = 4

    init(recombiner: Recombiner, width: Int, height: Int, dual: Bool) throws {
        self.recombiner = recombiner
        self.width = width
        self.height = height
        self.dual = dual
        aux = dual ? H264Decoder() : nil
        pool = TexturePool(try (0..<FramePipeline.poolSize).map { _ in try recombiner.makeOutput(width: width, height: height) })
    }

    /// Decode one frame. nil when the access units gave no picture, or when
    /// every texture is still on its way to the screen (latest wins: the
    /// display is behind, so this picture is dropped, not waited for).
    func decode(main mainUnit: Data, aux auxUnit: Data) throws -> GPUFrame? {
        let results = DecodeResults()
        let main = self.main, aux = self.aux
        // The two streams are independent: decode them at the same time.
        DispatchQueue.concurrentPerform(iterations: aux == nil ? 1 : 2) { stream in
            do {
                let picture = stream == 0 ? try main.decode(mainUnit) : try aux!.decode(auxUnit)
                results.set(stream, picture)
            } catch {
                results.fail(error)
            }
        }
        if let failure = results.failure { throw failure }
        guard let mainPicture = results.pictures[0], !dual || results.pictures[1] != nil else { return nil }
        guard let texture = pool.take() else { return nil }
        let frame = GPUFrame(texture: texture, pool: pool)
        try recombiner.recombine(main: mainPicture, aux: results.pictures[1], into: texture)
        return frame
    }
}

/// Draws the latest GPUFrame into a CAMetalLayer that covers the picture.
final class GPUPresenter {
    let layer = CAMetalLayer()
    private let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private let nearest: MTLSamplerState
    private let linear: MTLSamplerState
    var current: GPUFrame?

    init(device: MTLDevice) throws {
        guard let queue = device.makeCommandQueue() else { throw GPUVideoError.noDevice }
        self.queue = queue
        let library = try device.makeLibrary(source: GPUPresenter.shader, options: nil)
        let descriptor = MTLRenderPipelineDescriptor()
        descriptor.vertexFunction = library.makeFunction(name: "frame_vertex")
        descriptor.fragmentFunction = library.makeFunction(name: "frame_fragment")
        descriptor.colorAttachments[0].pixelFormat = .bgra8Unorm
        pipeline = try device.makeRenderPipelineState(descriptor: descriptor)
        func sampler(_ filter: MTLSamplerMinMagFilter) -> MTLSamplerState? {
            let d = MTLSamplerDescriptor(); d.minFilter = filter; d.magFilter = filter
            return device.makeSamplerState(descriptor: d)
        }
        guard let n = sampler(.nearest), let l = sampler(.linear) else { throw GPUVideoError.noDevice }
        nearest = n; linear = l
        layer.device = device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.isOpaque = true
        layer.isHidden = true
        layer.actions = ["bounds": NSNull(), "position": NSNull(), "contents": NSNull(), "hidden": NSNull()]
    }

    /// Draw the current frame, stretched over the whole layer: 1:1 pixels
    /// sample texel centres exactly (nearest); a scaled picture is filtered.
    func draw() {
        guard let frame = current, !layer.isHidden, let drawable = layer.nextDrawable(),
              let commands = queue.makeCommandBuffer() else { return }
        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = drawable.texture
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        pass.colorAttachments[0].storeAction = .store
        guard let encoder = commands.makeRenderCommandEncoder(descriptor: pass) else { return }
        let exact = drawable.texture.width == frame.texture.width && drawable.texture.height == frame.texture.height
        encoder.setRenderPipelineState(pipeline)
        encoder.setFragmentTexture(frame.texture, index: 0)
        encoder.setFragmentSamplerState(exact ? nearest : linear, index: 0)
        encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        encoder.endEncoding()
        commands.present(drawable)
        // The texture goes back to the pool only once the GPU has read it.
        commands.addCompletedHandler { _ in _ = frame }
        commands.commit()
    }

    static let shader = """
    #include <metal_stdlib>
    using namespace metal;
    struct Out { float4 position [[position]]; float2 uv; };
    vertex Out frame_vertex(uint id [[vertex_id]]) {
        float2 uv = float2(id & 1, id >> 1);
        Out out;
        out.position = float4(uv.x * 2 - 1, 1 - uv.y * 2, 0, 1);
        out.uv = uv;
        return out;
    }
    fragment float4 frame_fragment(Out in [[stage_in]], texture2d<float> tex [[texture(0)]],
                                   sampler s [[sampler(0)]]) {
        return tex.sample(s, in.uv);
    }
    """
}

/// The viewer's decode side. `decode` runs on the command-reader thread, so a
/// slow decode slows the pipe and, through it, the client's reads: the
/// connection itself is the backpressure. Finished frames go to the main
/// thread latest-wins; each packet is answered with `decoded` (the client
/// acknowledges the frame to the server) or `decode_error`.
final class GPUVideo {
    let device: MTLDevice
    private let recombiner: Recombiner
    private var pipeline: FramePipeline?
    private let lock = NSLock()
    private var pending: GPUFrame?
    private var scheduled = false
    /// Called on the main thread when a frame is waiting.
    var onFrame: ((GPUFrame) -> Void)?
    /// Test hook (scripts/macos-video-check.py): GLIFF_VIDEO_PROBE=1 reads a
    /// few decoded pixels back into each `decoded` event.
    static let probing = ProcessInfo.processInfo.environment["GLIFF_VIDEO_PROBE"] == "1"
    private lazy var probeQueue = device.makeCommandQueue()

    /// Hardware decode needs a Metal device and a hardware H.264 decoder
    /// (absent in some virtual machines).
    static func make() -> GPUVideo? {
        guard VTIsHardwareDecodeSupported(kCMVideoCodecType_H264), let device = MTLCreateSystemDefaultDevice() else { return nil }
        do { return try GPUVideo(device: device) } catch { diagnostic("GPU video unavailable: \(error)"); return nil }
    }

    private init(device: MTLDevice) throws {
        self.device = device
        recombiner = try Recombiner(device: device)
    }

    func configure(width: Int, height: Int, dual: Bool) throws {
        guard width > 0, height > 0, width <= 8192, height <= 8192 else { throw GPUVideoError.malformed("size \(width)x\(height)") }
        pipeline = try FramePipeline(recombiner: recombiner, width: width, height: height, dual: dual)
    }

    /// Packet: frame_id u64 LE, main_len u32 LE, aux_len u32 LE, main, aux.
    func decode(packet data: Data) {
        guard data.count >= 16 else { event(["type": "decode_error", "message": "short packet"]); return }
        let id = UInt64(number(data, 0)) | UInt64(number(data, 4)) << 32
        let mainLen = Int(number(data, 8)), auxLen = Int(number(data, 12))
        guard 16 + mainLen + auxLen == data.count else {
            event(["type": "decode_error", "frame_id": id, "message": "packet lengths disagree"]); return
        }
        guard let pipeline = pipeline else {
            event(["type": "decode_error", "frame_id": id, "message": "video before configuration"]); return
        }
        let base = data.startIndex
        let main = data.subdata(in: (base + 16)..<(base + 16 + mainLen))
        let aux = data.subdata(in: (base + 16 + mainLen)..<(base + 16 + mainLen + auxLen))
        let started = DispatchTime.now().uptimeNanoseconds
        do {
            let frame = try pipeline.decode(main: main, aux: aux)
            // Decode and recombine only; the probe below is not timed.
            let ms = Double(DispatchTime.now().uptimeNanoseconds - started) / 1_000_000
            var reply: [String: Any] = ["type": "decoded", "frame_id": id, "ms": ms]
            if let frame = frame {
                if GPUVideo.probing { reply["probe"] = probe(frame.texture) }
                post(frame)
            }
            event(reply)
        } catch {
            event(["type": "decode_error", "frame_id": id, "message": "\(error)"])
        }
    }

    /// The points video_fixture.rs samples: the four stripe centres, then an
    /// even and an odd column of the alternating half.
    static func probePoints(_ w: Int, _ h: Int) -> [(Int, Int)] {
        let x0 = (w / 2) & ~1
        return [(w / 8, h / 4), (3 * w / 8, h / 4), (5 * w / 8, h / 4), (7 * w / 8, h / 4), (x0, 3 * h / 4), (x0 + 1, 3 * h / 4)]
    }

    /// RGB at the probe points, copied out of the (GPU-private) texture.
    private func probe(_ texture: MTLTexture) -> [[Int]] {
        let points = GPUVideo.probePoints(texture.width, texture.height)
        guard let queue = probeQueue, let buffer = device.makeBuffer(length: 4 * points.count, options: .storageModeShared),
              let commands = queue.makeCommandBuffer(), let blit = commands.makeBlitCommandEncoder() else { return [] }
        for (i, point) in points.enumerated() {
            blit.copy(from: texture, sourceSlice: 0, sourceLevel: 0,
                      sourceOrigin: MTLOrigin(x: point.0, y: point.1, z: 0), sourceSize: MTLSize(width: 1, height: 1, depth: 1),
                      to: buffer, destinationOffset: 4 * i, destinationBytesPerRow: 4, destinationBytesPerImage: 4)
        }
        blit.endEncoding()
        commands.commit()
        commands.waitUntilCompleted()
        let bytes = buffer.contents().bindMemory(to: UInt8.self, capacity: 4 * points.count)
        // bgra8Unorm: B, G, R, A.
        return points.indices.map { i in [Int(bytes[4 * i + 2]), Int(bytes[4 * i + 1]), Int(bytes[4 * i])] }
    }

    private func post(_ frame: GPUFrame) {
        lock.lock()
        pending = frame
        let schedule = !scheduled
        scheduled = true
        lock.unlock()
        if schedule {
            DispatchQueue.main.async { [weak self] in
                guard let self = self else { return }
                self.lock.lock(); let frame = self.pending; self.pending = nil; self.scheduled = false; self.lock.unlock()
                if let frame = frame { self.onFrame?(frame) }
            }
        }
    }
}

final class RemoteView: NSView {
    // The picture comes from the CPU path (image, drawn below) or the GPU
    // path (gpuSize, drawn by `presenter`'s CAMetalLayer over this view).
    var image: CGImage? {
        didSet { window?.invalidateCursorRects(for: self) }
    }
    var gpuSize: CGSize? {
        didSet { if gpuSize != oldValue { window?.invalidateCursorRects(for: self); placeGPU() } }
    }
    var presenter: GPUPresenter? {
        didSet {
            oldValue?.layer.removeFromSuperlayer()
            if let presenter = presenter { wantsLayer = true; layer?.addSublayer(presenter.layer); placeGPU() }
        }
    }
    /// The picture's size in stream pixels, from whichever path shows it.
    var pictureSize: CGSize? {
        if let image = image { return CGSize(width: image.width, height: image.height) }
        return gpuSize
    }
    var hasPicture: Bool { pictureSize != nil }
    private let invisibleCursor = NSCursor(image: NSImage(size: NSSize(width: 1, height: 1), flipped: false, drawingHandler: { _ in true }), hotSpot: .zero)
    var cursorInVideo = false {
        didSet { window?.invalidateCursorRects(for: self) }
    }
    var scale: Double = 1
    var pressed: Set<UInt16> = []
    var buttons: Set<Int> = []
    var released = false
    var hovering = false
    var message = "Connecting…"
    var tracking: NSTrackingArea?
    override var acceptsFirstResponder: Bool { true }
    override var isFlipped: Bool { true }
    override func updateTrackingAreas() {
        if let old = tracking { removeTrackingArea(old) }
        let area = NSTrackingArea(rect: .zero, options: [.activeInKeyWindow, .inVisibleRect, .mouseMoved, .mouseEnteredAndExited], owner: self)
        addTrackingArea(area); tracking = area
        super.updateTrackingAreas()
    }
    override func resetCursorRects() {
        super.resetCursorRects()
        if hasPicture && cursorInVideo { addCursorRect(pictureRect, cursor: invisibleCursor) }
    }
    var pictureRect: NSRect {
        guard let size = pictureSize, size.width > 0, size.height > 0 else { return .zero }
        let ratio = min(bounds.width / Double(size.width), bounds.height / Double(size.height))
        let w = Double(size.width) * ratio, h = Double(size.height) * ratio
        return NSRect(x: (bounds.width - w)/2, y: (bounds.height - h)/2, width: w, height: h)
    }
    /// Keep the Metal layer over the picture, sized in device pixels. The
    /// picture is centred, so the rect is the same in flipped or unflipped
    /// layer coordinates.
    func placeGPU() {
        guard let presenter = presenter else { return }
        CATransaction.begin(); CATransaction.setDisableActions(true)
        let show = gpuSize != nil && image == nil
        presenter.layer.isHidden = !show
        if show {
            let rect = pictureRect, factor = window?.backingScaleFactor ?? 1
            presenter.layer.frame = rect
            presenter.layer.contentsScale = factor
            presenter.layer.drawableSize = CGSize(width: max(1, (rect.width * factor).rounded()), height: max(1, (rect.height * factor).rounded()))
        }
        CATransaction.commit()
        if show { presenter.draw() }
    }
    override func layout() { super.layout(); placeGPU() }
    override func viewDidChangeBackingProperties() { super.viewDidChangeBackingProperties(); placeGPU() }
    override func draw(_ dirty: NSRect) {
        NSColor.black.setFill(); bounds.fill()
        if let image = image, let cg = NSGraphicsContext.current?.cgContext {
            cg.saveGState(); cg.interpolationQuality = .none
            let r = pictureRect
            cg.translateBy(x: r.minX, y: r.maxY); cg.scaleBy(x: 1, y: -1)
            cg.draw(image, in: CGRect(x: 0, y: 0, width: r.width, height: r.height)); cg.restoreGState()
        } else if gpuSize == nil {
            let attrs: [NSAttributedString.Key: Any] = [.foregroundColor: NSColor.white, .font: NSFont.systemFont(ofSize: 16)]
            let text = message as NSString
            let size = text.size(withAttributes: attrs)
            text.draw(in: NSRect(x: 24, y: bounds.height/2, width: max(bounds.width-48, size.width), height: 100), withAttributes: attrs)
        }
    }
    func release() {
        for key in pressed { event(["type": "key", "keycode": key, "pressed": false]) }
        for button in buttons { event(["type": "button", "button": button, "pressed": false]) }
        pressed.removeAll(); buttons.removeAll()
    }
    func key(_ e: NSEvent, _ down: Bool) {
        if down && e.keyCode == 53 && e.modifierFlags.contains(.shift) { release(); released = true; return }
        guard hovering, !released, !e.isARepeat else { return }
        if down { pressed.insert(e.keyCode) } else { pressed.remove(e.keyCode) }
        event(["type": "key", "keycode": e.keyCode, "pressed": down])
    }
    override func keyDown(with e: NSEvent) { key(e, true) }
    override func keyUp(with e: NSEvent) { key(e, false) }
    override func flagsChanged(with e: NSEvent) {
        guard hovering, !released else { return }
        // Caps Lock sends flagsChanged on both toggle directions, rather than
        // ordinary key-down/up events. Each transition must toggle the host.
        if e.keyCode == 57 {
            event(["type": "key", "keycode": 57, "pressed": true])
            event(["type": "key", "keycode": 57, "pressed": false])
            return
        }
        let mask: NSEvent.ModifierFlags
        switch e.keyCode {
        case 54, 55: mask = .command
        case 56, 60: mask = .shift
        case 58, 61: mask = .option
        case 59, 62: mask = .control
        default: return
        }
        let down = e.modifierFlags.contains(mask) && !pressed.contains(e.keyCode)
        if down { pressed.insert(e.keyCode) } else { pressed.remove(e.keyCode) }
        event(["type": "key", "keycode": e.keyCode, "pressed": down])
    }
    func motion(_ e: NSEvent) {
        let p = convert(e.locationInWindow, from: nil), r = pictureRect
        guard let size = pictureSize, r.contains(p) else {
            if hovering { release() }
            hovering = false
            return
        }
        hovering = true
        event(["type": "motion", "x": (p.x-r.minX)/r.width*Double(size.width)/scale, "y": (p.y-r.minY)/r.height*Double(size.height)/scale])
    }
    override func mouseMoved(with e: NSEvent) { motion(e) }
    override func mouseDragged(with e: NSEvent) { motion(e) }
    override func rightMouseDragged(with e: NSEvent) { motion(e) }
    override func otherMouseDragged(with e: NSEvent) { motion(e) }
    override func mouseEntered(with e: NSEvent) { hovering = pictureRect.contains(convert(e.locationInWindow, from: nil)); released = false }
    override func mouseExited(with e: NSEvent) { hovering = false; release() }
    func button(_ e: NSEvent, _ down: Bool) {
        if down { hovering = true; released = false; window?.makeFirstResponder(self); buttons.insert(e.buttonNumber) } else { buttons.remove(e.buttonNumber) }
        motion(e); event(["type": "button", "button": e.buttonNumber, "pressed": down])
    }
    override func mouseDown(with e: NSEvent) { button(e, true) }
    override func mouseUp(with e: NSEvent) { button(e, false) }
    override func rightMouseDown(with e: NSEvent) { button(e, true) }
    override func rightMouseUp(with e: NSEvent) { button(e, false) }
    override func otherMouseDown(with e: NSEvent) { button(e, true) }
    override func otherMouseUp(with e: NSEvent) { button(e, false) }
    override func scrollWheel(with e: NSEvent) {
        guard pictureRect.contains(convert(e.locationInWindow, from: nil)) else { return }
        let factor = e.hasPreciseScrollingDeltas ? 1.0 : 10.0
        if e.scrollingDeltaY != 0 { event(["type": "axis", "horizontal": false, "value": -e.scrollingDeltaY * factor]) }
        if e.scrollingDeltaX != 0 { event(["type": "axis", "horizontal": true, "value": -e.scrollingDeltaX * factor]) }
    }
}

final class App: NSObject, NSApplicationDelegate, NSWindowDelegate {
    let view = RemoteView()
    var window: NSWindow?
    // The Machine menu: the everyday per-machine choices, saved through the
    // client like the settings window (see crates/gliff-macos/src/settings.rs).
    let mirrorItem = NSMenuItem(title: "Mirror Screen", action: #selector(App.mirrorScreen), keyEquivalent: "")
    let privateItem = NSMenuItem(title: "Private Screen", action: #selector(App.privateScreen), keyEquivalent: "")
    let clipboardItem = NSMenuItem(title: "Share Clipboard", action: #selector(App.toggleClipboard), keyEquivalent: "")
    var shareClipboard = true
    // Machine > Play Sound: the shared mute switch; enabled when the host sends sound.
    let soundItem = NSMenuItem(title: "Play Sound", action: #selector(App.toggleSound), keyEquivalent: "")
    var playSound = true
    // Hardware decode (VideoToolbox + Metal) when this Mac has it; the
    // client then sends coded video (kind 5) instead of BGRA pixels (kind 1).
    let gpu = GPUVideo.make()
    var pending: Data?
    let pendingLock = NSLock()
    var drawing = false
    var painted = false
    func applicationDidFinishLaunching(_ note: Notification) {
        // The client reads this first, before it says Hello (it decides the
        // chroma it asks for and how it sends frames).
        if let gpu = gpu, let presenter = try? GPUPresenter(device: gpu.device) {
            view.presenter = presenter
            gpu.onFrame = { [weak self] frame in self?.showGPU(frame) }
            event(["type": "video_caps", "hardware": true])
        } else {
            event(["type": "video_caps", "hardware": false])
        }
        let menu = NSMenu(); let appItem = NSMenuItem(); menu.addItem(appItem)
        let appMenu = NSMenu(); appItem.submenu = appMenu
        let hosting = appMenu.addItem(withTitle: "Enable Remote Control…", action: #selector(App.requestHostPermissions), keyEquivalent: "")
        hosting.target = self
        let settings = appMenu.addItem(withTitle: "Settings…", action: #selector(App.openSettings), keyEquivalent: ",")
        settings.target = self
        appMenu.addItem(NSMenuItem.separator())
        appMenu.addItem(withTitle: "Quit Gliff", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        let machineItem = NSMenuItem(); menu.addItem(machineItem)
        let machineMenu = NSMenu(title: "Machine"); machineItem.submenu = machineMenu
        // Enabled state is set from machine_state (screen_locked), not validation.
        machineMenu.autoenablesItems = false
        for item in [mirrorItem, privateItem] { item.target = self; machineMenu.addItem(item) }
        machineMenu.addItem(NSMenuItem.separator())
        clipboardItem.target = self; machineMenu.addItem(clipboardItem)
        soundItem.target = self; soundItem.isEnabled = false; machineMenu.addItem(soundItem)
        machineMenu.addItem(NSMenuItem.separator())
        let reconnect = machineMenu.addItem(withTitle: "Reconnect", action: #selector(App.reconnect), keyEquivalent: "")
        reconnect.target = self
        NSApplication.shared.mainMenu = menu
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1100, height: 720), styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        w.title = CommandLine.arguments.dropFirst(2).first ?? "Gliff"
        w.contentView = view; w.delegate = self; w.acceptsMouseMovedEvents = true
        w.center(); w.makeKeyAndOrderFront(nil); w.makeFirstResponder(view); window = w
        NSApplication.shared.activate(ignoringOtherApps: true)
        clipboard.start()
        keyboardLayoutChanged(nil)
        DistributedNotificationCenter.default().addObserver(self, selector: #selector(keyboardLayoutChanged(_:)), name: Notification.Name(kTISNotifySelectedKeyboardInputSourceChanged as String), object: nil)
        resizeEvent()
        NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp]) { [weak self] e in
            guard let self = self, self.window?.isKeyWindow == true, !self.view.released, self.view.hovering, self.view.hasPicture else { return e }
            self.view.key(e, e.type == .keyDown); return nil
        }
        readCommands { [weak self] kind, data in
            guard let self = self else { return }
            if kind == 5 {
                // Coded video: decode here, on the reader thread, in order.
                if let gpu = self.gpu { gpu.decode(packet: data) } else { event(["type": "gpu_failed", "message": "no hardware decoder"]) }
                return
            }
            if kind == 1 {
                self.pendingLock.lock(); self.pending = data
                let schedule = !self.drawing; self.drawing = true; self.pendingLock.unlock()
                if schedule { DispatchQueue.main.async { autoreleasepool { self.drawPending() } } }
            } else {
                let cmd = json(data)
                if clipboard.receive(cmd) { return }
                if cmd["type"] as? String == "video_config" { self.configureVideo(cmd); return }
                DispatchQueue.main.async {
                    clipboard.command(cmd)
                    if cmd["type"] as? String == "cursor_mode" { self.view.cursorInVideo = cmd["embedded"] as? Bool == true }
                    // Pointer scale for the GPU path (the CPU path gets it with each frame).
                    if cmd["type"] as? String == "video_scale", let milli = cmd["scale_milli"] as? Int, milli > 0 { self.view.scale = Double(milli) / 1000 }
                    if cmd["type"] as? String == "machine_state" { self.applyMachineState(cmd) }
                    if cmd["type"] as? String == "report_size" { self.resizeEvent() }
                    if let message = cmd["message"] as? String {
                        self.view.message = message; self.view.image = nil
                        self.view.presenter?.current = nil; self.view.gpuSize = nil; self.view.needsDisplay = true
                    }
                    if let text = cmd["clipboard"] as? String { NSPasteboard.general.clearContents(); NSPasteboard.general.setString(text, forType: .string) }
                }
            }
        }
    }
    /// A new coded stream: size and whether an auxiliary (Dual420) stream
    /// comes with it. Runs on the reader thread, ahead of that stream's frames.
    func configureVideo(_ cmd: [String: Any]) {
        guard let gpu = gpu else { event(["type": "gpu_failed", "message": "no hardware decoder"]); return }
        let width = cmd["width"] as? Int ?? 0, height = cmd["height"] as? Int ?? 0
        let dual = cmd["dual"] as? Bool == true
        let scale = Double(cmd["scale_milli"] as? Int ?? 1000) / 1000
        do {
            try gpu.configure(width: width, height: height, dual: dual)
            DispatchQueue.main.async { self.view.scale = scale }
            let chroma = dual ? "4:4:4" : "4:2:0"
            diagnostic("Gliff video: VideoToolbox + Metal, \(width)x\(height) \(chroma)")
        } catch {
            event(["type": "gpu_failed", "message": "\(error)"])
        }
    }
    func showGPU(_ frame: GPUFrame) {
        if view.image != nil { view.image = nil }
        view.presenter?.current = frame
        let size = CGSize(width: frame.texture.width, height: frame.texture.height)
        if view.gpuSize != size { view.gpuSize = size; view.needsDisplay = true } else { view.presenter?.draw() }
        if !painted { painted = true; diagnostic("Gliff displayed first frame: \(frame.texture.width)x\(frame.texture.height) (GPU)") }
    }
    func applyMachineState(_ cmd: [String: Any]) {
        let privateScreen = cmd["screen"] as? String == "private"
        let locked = cmd["screen_locked"] as? Bool == true
        mirrorItem.state = privateScreen ? .off : .on
        privateItem.state = privateScreen ? .on : .off
        mirrorItem.isEnabled = !locked; privateItem.isEnabled = !locked
        if locked { mirrorItem.toolTip = "--output names the screen for this connection"; privateItem.toolTip = mirrorItem.toolTip }
        shareClipboard = cmd["clipboard"] as? Bool != false
        clipboardItem.state = shareClipboard ? .on : .off
        if let sound = cmd["sound"] as? Bool {
            playSound = sound
            soundItem.state = sound ? .on : .off
            let available = cmd["sound_available"] as? Bool == true
            soundItem.isEnabled = available
            soundItem.toolTip = available ? nil : "This computer does not send sound (sound comes from Linux hosts)"
        }
    }
    @objc func openSettings() { event(["type": "open_settings"]) }
    @objc func mirrorScreen() { event(["type": "setting", "field": "screen", "value": "mirror"]) }
    @objc func privateScreen() { event(["type": "setting", "field": "screen", "value": "private"]) }
    @objc func toggleClipboard() { event(["type": "setting", "field": "clipboard", "value": !shareClipboard]) }
    @objc func toggleSound() { event(["type": "sound", "on": !playSound]) }
    @objc func reconnect() { event(["type": "reconnect"]) }
    @objc func requestHostPermissions() {
        _ = CGRequestScreenCaptureAccess()
        _ = AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary)
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture") { NSWorkspace.shared.open(url) }
    }
    func drawPending() {
        pendingLock.lock(); let data = pending; pending = nil; drawing = false; pendingLock.unlock()
        guard let data = data, data.count >= 12 else { return }
        let w = Int(number(data, 0)), h = Int(number(data, 4)), s = number(data, 8)
        guard w > 0, h > 0, s > 0, w * h * 4 == data.count - 12, let provider = CGDataProvider(data: data.subdata(in: 12..<data.count) as CFData) else { return }
        view.image = CGImage(width: w, height: h, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: w * 4, space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipFirst.rawValue).union(.byteOrder32Little), provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent)
        view.scale = Double(s)/1000; view.needsDisplay = true
        if view.gpuSize != nil { view.presenter?.current = nil; view.gpuSize = nil }
        if !painted { painted = true; diagnostic("Gliff displayed first frame: \(w)x\(h)") }
    }
    func resizeEvent() {
        let factor = window?.backingScaleFactor ?? 1
        event(["type": "resize", "width": Int(view.bounds.width * factor), "height": Int(view.bounds.height * factor), "scale": factor])
    }
    @objc func keyboardLayoutChanged(_ note: Notification?) {
        if let layout = keyboardLayout() { event(layout) }
    }
    func windowDidResize(_ note: Notification) { resizeEvent() }
    func windowDidEnterFullScreen(_ note: Notification) { resizeEvent() }
    func windowDidExitFullScreen(_ note: Notification) { resizeEvent() }
    func windowDidChangeBackingProperties(_ note: Notification) { resizeEvent() }
    func windowDidResignKey(_ note: Notification) { view.release() }
    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { true }
    func applicationWillTerminate(_ note: Notification) { DistributedNotificationCenter.default().removeObserver(self); view.release(); event(["type": "bye"]) }
}

struct NearbyComputer {
    let id: String
    let name: String
    let host: String
    let port: Int
    let user: String
    let platform: String
    var address: String { user.isEmpty ? host : "\(user)@\(host)" }
    var displayName: String {
        let hostname = host.hasSuffix(".local") ? String(host.dropLast(6)) : host
        return name == "Gliff on \(hostname) (\(user))" ? hostname : name
    }
    var value: [String: Any] { ["name": name, "host": host, "port": port, "user": user, "platform": platform] }
}

// Bonjour supplies additions/removals rather than repeated subnet scans.
// Keep at most 128 service objects and resolve names only, not SSH connections.
final class NearbyComputers: NSObject, NetServiceBrowserDelegate, NetServiceDelegate {
    let browser = NetServiceBrowser()
    var services: [String: NetService] = [:]
    var records: [String: NearbyComputer] = [:]
    var onChange: (() -> Void)?
    var failure: String?
    var computers: [NearbyComputer] {
        records.values.sorted { ($0.name, $0.address) < ($1.name, $1.address) }
    }
    func key(_ service: NetService) -> String { "\(service.name)\n\(service.type)\n\(service.domain)" }
    func start() {
        stop(); failure = nil
        browser.delegate = self
        browser.searchForServices(ofType: "_gliff._tcp.", inDomain: "local.")
        onChange?()
    }
    func stop() {
        browser.stop()
        for service in services.values { service.stopMonitoring(); service.stop(); service.delegate = nil }
        services.removeAll(); records.removeAll()
    }
    func netServiceBrowser(_ browser: NetServiceBrowser, didFind service: NetService, moreComing: Bool) {
        let id = key(service)
        guard services[id] == nil, services.count < 128 else { return }
        services[id] = service; service.delegate = self
        service.resolve(withTimeout: 5)
    }
    func netServiceBrowser(_ browser: NetServiceBrowser, didRemove service: NetService, moreComing: Bool) {
        let id = key(service)
        if let old = services.removeValue(forKey: id) {
            old.stopMonitoring(); old.stop(); old.delegate = nil
        }
        records.removeValue(forKey: id)
        onChange?()
    }
    func netServiceBrowser(_ browser: NetServiceBrowser, didNotSearch errorDict: [String: NSNumber]) {
        failure = "Nearby computers unavailable. Allow Gliff in System Settings → Privacy & Security → Local Network, or enter an SSH address."
        onChange?()
    }
    func netServiceDidResolveAddress(_ service: NetService) {
        update(service); service.startMonitoring()
    }
    func netService(_ service: NetService, didUpdateTXTRecord data: Data) { update(service) }
    func netService(_ service: NetService, didNotResolve errorDict: [String: NSNumber]) {
        records.removeValue(forKey: key(service)); onChange?()
    }
    func update(_ service: NetService) {
        let id = key(service)
        guard services[id] === service else { return }
        let txt = NetService.dictionary(fromTXTRecord: service.txtRecordData() ?? Data())
        func text(_ key: String) -> String { txt[key].flatMap { String(data: $0, encoding: .utf8) } ?? "" }
        let host = (service.hostName ?? "").trimmingCharacters(in: CharacterSet(charactersIn: "."))
        guard text("transport") == "ssh", text("protocol") == "1", (1...65535).contains(service.port),
              host.range(of: "^[A-Za-z0-9_][A-Za-z0-9_.-]{0,252}$", options: .regularExpression) != nil else {
            records.removeValue(forKey: id); onChange?(); return
        }
        let hint = text("user")
        let user = hint.range(of: "^[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}$", options: .regularExpression) != nil ? hint : ""
        records[id] = NearbyComputer(id: id, name: service.name, host: host, port: service.port, user: user, platform: text("platform"))
        onChange?()
    }
}

final class ComputerPickerRow: NSTableRowView {
    override func drawSelection(in dirtyRect: NSRect) {
        guard selectionHighlightStyle != .none else { return }
        let color = isEmphasized ? NSColor.selectedContentBackgroundColor : NSColor.unemphasizedSelectedContentBackgroundColor
        color.setFill()
        var rect = bounds.insetBy(dx: 4, dy: 3)
        if let scroll = enclosingScrollView { rect.size.width = min(bounds.width, scroll.contentSize.width) - 8 }
        NSBezierPath(roundedRect: rect, xRadius: 6, yRadius: 6).fill()
    }
}

final class ComputerPicker: NSObject, NSApplicationDelegate, NSWindowDelegate, NSTableViewDataSource, NSTableViewDelegate, NSTextFieldDelegate {
    let nearby = NearbyComputers()
    let table = NSTableView()
    let address = NSTextField(string: "")
    let port = NSTextField(string: "22")
    let status = NSTextField(wrappingLabelWithString: "Searching the local network…")
    let progress = NSProgressIndicator()
    let screen = NSPopUpButton()
    let screenDetail = NSTextField(wrappingLabelWithString: "")
    let preferences: UserDefaults
    let screenOverride: String?
    var preferenceTarget: String?
    var window: NSWindow?
    var listHeight: NSLayoutConstraint?
    var rows: [NearbyComputer] = []
    var updating = false
    var finished = false
    var selectedID: String?
    // Saved settings from the client (machine_settings): the resolved screen
    // per known machine, which machines set one themselves, and the mode for
    // an address with no settings yet.
    var configScreens: [String: String] = [:]
    var pinned: Set<String> = []
    var defaultScreen = "mirror"
    // Choices made in this window, per SSH address and port, so switching
    // between addresses keeps them; the one connected to is saved for the
    // machine. legacyUsed: the menu shows a pre-settings UserDefaults choice,
    // which is also saved (moved) on Connect.
    var sessionScreens: [String: String] = [:]
    var legacyUsed = false
    init(screenOverride: String? = nil, preferences: UserDefaults? = nil) {
        self.screenOverride = screenOverride
        if let preferences = preferences { self.preferences = preferences }
        else if Bundle.main.bundleIdentifier == "org.omacom.gliff" { self.preferences = .standard }
        else { self.preferences = UserDefaults(suiteName: "org.omacom.gliff") ?? .standard }
        super.init()
    }
    func label(_ text: String, _ size: CGFloat = 13) -> NSTextField {
        let label = NSTextField(labelWithString: text)
        label.font = NSFont.systemFont(ofSize: size); return label
    }
    func stack(_ views: [NSView], _ orientation: NSUserInterfaceLayoutOrientation, _ spacing: CGFloat) -> NSStackView {
        let stack = NSStackView(views: views)
        stack.orientation = orientation; stack.spacing = spacing
        stack.alignment = orientation == .vertical ? .leading : .centerY
        stack.translatesAutoresizingMaskIntoConstraints = false
        return stack
    }
    func applicationDidFinishLaunching(_ note: Notification) {
        let menu = NSMenu(); let item = NSMenuItem(); menu.addItem(item)
        let appMenu = NSMenu(); item.submenu = appMenu
        let permission = appMenu.addItem(withTitle: "Enable Remote Control…", action: #selector(hostPermissions), keyEquivalent: "")
        permission.target = self
        let settingsItem = appMenu.addItem(withTitle: "Settings…", action: #selector(openSettings), keyEquivalent: ",")
        settingsItem.target = self
        appMenu.addItem(NSMenuItem.separator())
        appMenu.addItem(withTitle: "Quit Gliff", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        NSApplication.shared.mainMenu = menu
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 340), styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        w.title = "Gliff"; w.delegate = self
        w.backgroundColor = .windowBackgroundColor
        w.titlebarAppearsTransparent = true
        let view = w.contentView!
        let title = label("Connect to a Computer", 18)
        title.font = NSFont.systemFont(ofSize: 18, weight: .semibold)
        let subtitle = label("Choose a nearby computer or enter an SSH address.")
        subtitle.textColor = .secondaryLabelColor
        let heading = stack([title, subtitle], .vertical, 4)
        let list = NSBox()
        list.boxType = .custom; list.titlePosition = .noTitle
        list.fillColor = .controlBackgroundColor; list.borderColor = .separatorColor
        list.borderWidth = 0.5; list.cornerRadius = 8; list.contentViewMargins = .zero
        list.wantsLayer = true; list.layer?.cornerRadius = 8; list.layer?.masksToBounds = true
        let scroll = NSScrollView()
        scroll.translatesAutoresizingMaskIntoConstraints = false
        scroll.hasVerticalScroller = true; scroll.borderType = .noBorder
        for (id, title, width) in [("name", "Computer", CGFloat(180)), ("address", "SSH Address", CGFloat(324))] {
            let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(id)); column.title = title; column.width = width
            table.addTableColumn(column)
        }
        table.delegate = self; table.dataSource = self; table.rowHeight = 30
        table.style = .inset; table.usesAlternatingRowBackgroundColors = true
        table.columnAutoresizingStyle = .lastColumnOnlyAutoresizingStyle
        table.setAccessibilityLabel("Nearby Computers")
        table.frame = NSRect(x: 0, y: 0, width: 536, height: 66); table.autoresizingMask = [.width]
        table.target = self; table.doubleAction = #selector(connect)
        scroll.documentView = table; list.contentView!.addSubview(scroll)
        progress.style = .spinning; progress.controlSize = .small
        progress.isIndeterminate = true; progress.isDisplayedWhenStopped = false
        progress.setAccessibilityLabel("Searching for nearby computers")
        status.font = NSFont.systemFont(ofSize: 12); status.textColor = .secondaryLabelColor
        status.preferredMaxLayoutWidth = 496
        status.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let refresh = NSButton(image: NSImage(systemSymbolName: "arrow.clockwise", accessibilityDescription: "Refresh Nearby Computers")!, target: self, action: #selector(refresh))
        refresh.bezelStyle = .inline; refresh.controlSize = .small
        refresh.toolTip = "Refresh Nearby Computers"; refresh.setAccessibilityLabel("Refresh Nearby Computers")
        let statusRow = stack([progress, status, refresh], .horizontal, 8)
        address.placeholderString = "user@computer.local"
        address.bezelStyle = .roundedBezel; port.bezelStyle = .roundedBezel
        address.font = NSFont.systemFont(ofSize: 13); port.font = address.font
        address.setAccessibilityLabel("SSH Address"); port.setAccessibilityLabel("SSH Port")
        address.setContentHuggingPriority(.defaultLow, for: .horizontal)
        address.delegate = self; port.delegate = self
        let fields = stack([label("SSH Address:"), address, label("Port:"), port], .horizontal, 10)
        screen.addItems(withTitles: ["Mirror screen", "Private screen"])
        screen.setAccessibilityLabel("Screen mode")
        screen.target = self; screen.action = #selector(screenChanged)
        screen.isEnabled = screenOverride == nil
        screenDetail.font = NSFont.systemFont(ofSize: 12); screenDetail.textColor = .secondaryLabelColor
        let screenRow = stack([label("Screen:"), screen, screenDetail], .horizontal, 10)
        screenDetail.setContentHuggingPriority(.defaultLow, for: .horizontal)
        screenDetail.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let cancel = NSButton(title: "Cancel", target: self, action: #selector(cancel))
        cancel.bezelStyle = .rounded; cancel.controlSize = .large; cancel.keyEquivalent = "\u{1b}"
        let connect = NSButton(title: "Connect", target: self, action: #selector(connect))
        connect.bezelStyle = .rounded; connect.controlSize = .large; connect.keyEquivalent = "\r"
        let settingsButton = NSButton(title: "Settings…", target: self, action: #selector(openSettings))
        settingsButton.bezelStyle = .rounded; settingsButton.controlSize = .large
        settingsButton.setAccessibilityLabel("Settings")
        let actions = stack([settingsButton, NSView(), cancel, connect], .horizontal, 8)
        let content = stack([heading, list, statusRow, fields, screenRow, actions], .vertical, 12)
        view.addSubview(content)
        for row in [heading, list, statusRow, fields, screenRow, actions] { row.translatesAutoresizingMaskIntoConstraints = false }
        let listHeight = list.heightAnchor.constraint(equalToConstant: 66)
        self.listHeight = listHeight
        NSLayoutConstraint.activate([
            view.widthAnchor.constraint(greaterThanOrEqualToConstant: 600),
            content.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 20),
            content.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -20),
            content.topAnchor.constraint(equalTo: view.topAnchor, constant: 20),
            content.bottomAnchor.constraint(equalTo: view.bottomAnchor, constant: -20),
            list.widthAnchor.constraint(equalTo: content.widthAnchor),
            listHeight,
            scroll.leadingAnchor.constraint(equalTo: list.contentView!.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: list.contentView!.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: list.contentView!.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: list.contentView!.bottomAnchor),
            statusRow.widthAnchor.constraint(equalTo: content.widthAnchor),
            statusRow.heightAnchor.constraint(greaterThanOrEqualToConstant: 20),
            progress.widthAnchor.constraint(equalToConstant: 16),
            progress.heightAnchor.constraint(equalToConstant: 16),
            fields.widthAnchor.constraint(equalTo: content.widthAnchor),
            port.widthAnchor.constraint(equalToConstant: 64),
            screenRow.widthAnchor.constraint(equalTo: content.widthAnchor),
            screen.widthAnchor.constraint(equalToConstant: 140),
            actions.widthAnchor.constraint(equalTo: content.widthAnchor),
            cancel.widthAnchor.constraint(greaterThanOrEqualToConstant: 80),
            connect.widthAnchor.constraint(greaterThanOrEqualToConstant: 90),
        ])
        table.nextKeyView = address; address.nextKeyView = port
        port.nextKeyView = screen; screen.nextKeyView = connect; connect.nextKeyView = cancel; cancel.nextKeyView = refresh
        refresh.nextKeyView = table
        w.defaultButtonCell = connect.cell as? NSButtonCell
        window = w; restoreScreenPreference(); fitWindow()
        w.center(); w.makeKeyAndOrderFront(nil); w.makeFirstResponder(address)
        NSApplication.shared.activate(ignoringOtherApps: true)
        nearby.onChange = { [weak self] in self?.update() }
        nearby.start()
        // The parent can close/inspect this chooser without making a connection.
        readCommands(onClose: { [weak self] in self?.cancel(nil) }) { [weak self] _, data in
            let cmd = json(data)
            DispatchQueue.main.async {
                guard let self = self else { return }
                if cmd["type"] as? String == "cancel" { self.cancel(nil) }
                if cmd["type"] as? String == "machine_settings" {
                    self.configScreens = cmd["screens"] as? [String: String] ?? [:]
                    self.pinned = Set(cmd["pinned"] as? [String] ?? [])
                    self.defaultScreen = cmd["default_screen"] as? String ?? "mirror"
                    // Choices made in this window still win (sessionScreens).
                    self.preferenceTarget = nil; self.restoreScreenPreference()
                }
                if cmd["type"] as? String == "select", let index = cmd["index"] as? Int, self.rows.indices.contains(index) {
                    self.table.selectRowIndexes(IndexSet(integer: index), byExtendingSelection: false)
                    self.window?.makeFirstResponder(self.table)
                }
                if cmd["type"] as? String == "inspect" {
                    event(["type": "picker_state", "hosts": self.rows.map { $0.value }, "rows": self.table.numberOfRows, "host": self.address.stringValue, "port": self.port.stringValue, "screen": self.screenMode])
                }
            }
        }
    }
    func update() {
        updating = true; rows = nearby.computers; table.reloadData()
        if let index = rows.firstIndex(where: { $0.id == selectedID }) {
            table.selectRowIndexes(IndexSet(integer: index), byExtendingSelection: false)
        }
        updating = false
        if rows.isEmpty && nearby.failure == nil { progress.startAnimation(nil) } else { progress.stopAnimation(nil) }
        status.stringValue = nearby.failure ?? (rows.isEmpty ? "Searching for nearby computers… You can also enter an SSH address." : "\(rows.count) computer\(rows.count == 1 ? "" : "s") found. SSH access is required to connect.")
        listHeight?.constant = (table.headerView?.frame.height ?? 22) + CGFloat(max(1, min(rows.count, 4))) * (table.rowHeight + table.intercellSpacing.height) + 12
        fitWindow()
    }
    func fitWindow() {
        guard let window = window, let view = window.contentView else { return }
        // Follow the content until the user enlarges the window; preserve their size afterwards.
        let wasCompact = window.contentMinSize == .zero || window.contentLayoutRect.size == window.contentMinSize
        view.layoutSubtreeIfNeeded()
        let size = view.fittingSize
        window.contentMinSize = size
        if wasCompact { window.setContentSize(size) }
    }
    func numberOfRows(in tableView: NSTableView) -> Int { rows.count }
    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? { ComputerPickerRow() }
    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard rows.indices.contains(row) else { return nil }
        let computer = rows[row]
        let identifier = tableColumn!.identifier
        if let cell = tableView.makeView(withIdentifier: identifier, owner: self) as? NSTableCellView {
            cell.textField?.stringValue = identifier.rawValue == "name" ? computer.displayName : computer.address
            return cell
        }
        let cell = NSTableCellView(); cell.identifier = identifier
        let text = label(identifier.rawValue == "name" ? computer.displayName : computer.address)
        text.lineBreakMode = .byTruncatingTail; text.translatesAutoresizingMaskIntoConstraints = false
        cell.textField = text; cell.addSubview(text)
        var leading = cell.leadingAnchor
        if identifier.rawValue == "name" {
            let icon = NSImageView()
            icon.image = NSImage(systemSymbolName: "desktopcomputer", accessibilityDescription: nil)
            icon.translatesAutoresizingMaskIntoConstraints = false
            cell.imageView = icon; cell.addSubview(icon)
            NSLayoutConstraint.activate([
                icon.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 4),
                icon.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
                icon.widthAnchor.constraint(equalToConstant: 18), icon.heightAnchor.constraint(equalToConstant: 18),
            ])
            leading = icon.trailingAnchor
        }
        NSLayoutConstraint.activate([
            text.leadingAnchor.constraint(equalTo: leading, constant: 8),
            text.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -6),
            text.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
        ])
        return cell
    }
    func tableViewSelectionDidChange(_ notification: Notification) {
        guard !updating, rows.indices.contains(table.selectedRow) else { return }
        let computer = rows[table.selectedRow]; selectedID = computer.id
        address.stringValue = computer.address; port.stringValue = String(computer.port)
        restoreScreenPreference()
    }
    var screenMode: String { screen.indexOfSelectedItem == 1 ? "private" : "mirror" }
    var screenPreferenceKey: String? {
        let host = address.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !host.isEmpty, let number = Int(port.stringValue), (1...65535).contains(number) else { return nil }
        return "screen.\(host):\(number)"
    }
    func restoreScreenPreference() {
        let key = screenPreferenceKey
        guard key != preferenceTarget || preferenceTarget == nil else { return }
        preferenceTarget = key
        let host = address.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        // Order: the command line, a choice made in this window, the machine's
        // saved setting, a choice from before settings existed (UserDefaults,
        // moved to the config on Connect), then the defaults.
        var mode = configScreens[host] ?? defaultScreen
        legacyUsed = false
        if let forced = screenOverride { mode = forced }
        else if let key = key, let chosen = sessionScreens[key] { mode = chosen }
        else if !pinned.contains(host), let key = key, let old = preferences.string(forKey: key) { mode = old; legacyUsed = true }
        screen.selectItem(at: mode == "private" ? 1 : 0)
        updateScreenDetail()
    }
    func updateScreenDetail() {
        screenDetail.stringValue = screenMode == "private" ? "A separate Linux desktop that follows this window." : "Shows the host desktop, keeping its proportions."
        fitWindow()
    }
    @objc func screenChanged(_ sender: Any?) {
        if let key = screenPreferenceKey { sessionScreens[key] = screenMode }
        updateScreenDetail()
    }
    @objc func openSettings(_ sender: Any?) { event(["type": "open_settings"]) }
    func controlTextDidChange(_ note: Notification) { restoreScreenPreference() }
    @objc func refresh(_ sender: Any?) { nearby.start() }
    func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        if commandSelector == #selector(NSResponder.insertNewline(_:)) { connect(nil); return true }
        if commandSelector == #selector(NSResponder.cancelOperation(_:)) { cancel(nil); return true }
        return false
    }
    @objc func connect(_ sender: Any?) {
        let host = address.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !host.isEmpty, !host.hasPrefix("-"), host.utf8.count <= 320,
              host.range(of: "^[A-Za-z0-9_.:@\\[\\]%-]+$", options: .regularExpression) != nil,
              let number = Int(port.stringValue), (1...65535).contains(number) else {
            status.stringValue = "Enter an SSH address and a port between 1 and 65535."; fitWindow(); return
        }
        // The client saves the choice in the settings; the old copy can go.
        let chosenHere = screenPreferenceKey.flatMap { sessionScreens[$0] } != nil
        let explicit = screenOverride == nil && (chosenHere || legacyUsed)
        if explicit, let key = screenPreferenceKey { preferences.removeObject(forKey: key) }
        complete(["type": "connect", "host": host, "port": number, "screen": screenMode, "screen_explicit": explicit])
    }
    @objc func cancel(_ sender: Any?) { complete(["type": "cancel"]) }
    func complete(_ value: [String: Any]) {
        guard !finished else { return }
        finished = true; nearby.stop(); event(value); NSApplication.shared.terminate(nil)
    }
    @objc func hostPermissions(_ sender: Any?) {
        _ = CGRequestScreenCaptureAccess()
        _ = AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary)
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture") { NSWorkspace.shared.open(url) }
    }
    func windowWillClose(_ note: Notification) { cancel(nil) }
    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { true }
    func applicationWillTerminate(_ note: Notification) {
        if !finished { finished = true; nearby.stop(); event(["type": "cancel"]) }
    }
}

// The settings window: the Mac counterpart of the Linux client's
// settings_ui.rs. "All Machines" is the [defaults] table; each machine is its
// [overrides."user@host"] table. The client owns config.toml: this window
// gets settings_state, sends settings_edit (a null value clears a field), and
// shows every change as soon as the client has saved it.
final class SettingsWindow: NSObject, NSApplicationDelegate, NSWindowDelegate, NSTableViewDataSource, NSTableViewDelegate, NSSearchFieldDelegate {
    var window: NSWindow?
    let search = NSSearchField()
    let table = NSTableView()
    let heading = NSTextField(labelWithString: "All Machines")
    let scopeNote = NSTextField(wrappingLabelWithString: "")
    let screen = NSPopUpButton()
    let clipboardBox = NSButton(checkboxWithTitle: "Share the clipboard", target: nil, action: nil)
    let serverBin = NSTextField(string: "")
    let screenOrigin = NSTextField(labelWithString: "")
    let clipboardOrigin = NSTextField(labelWithString: "")
    let serverOrigin = NSTextField(labelWithString: "")
    let screenReset = NSButton(title: "Reset", target: nil, action: nil)
    let clipboardReset = NSButton(title: "Reset", target: nil, action: nil)
    let serverReset = NSButton(title: "Reset", target: nil, action: nil)
    let pathLabel = NSTextField(wrappingLabelWithString: "")
    let errorLabel = NSTextField(wrappingLabelWithString: "")
    var state: [String: Any] = [:]
    // "" is All Machines; then the machines that match the search.
    var scopes: [String] = [""]
    var selected = ""
    var updating = false

    func label(_ text: String, secondary: Bool = false) -> NSTextField {
        let l = NSTextField(labelWithString: text)
        l.font = NSFont.systemFont(ofSize: secondary ? 11 : 13)
        if secondary { l.textColor = .secondaryLabelColor }
        return l
    }
    func row(_ title: String, _ control: NSView, _ origin: NSTextField, _ reset: NSButton, _ help: String) -> NSStackView {
        let name = label(title)
        name.alignment = .right
        name.widthAnchor.constraint(equalToConstant: 120).isActive = true
        origin.font = NSFont.systemFont(ofSize: 11); origin.textColor = .secondaryLabelColor
        reset.bezelStyle = .inline; reset.controlSize = .small; reset.target = self; reset.action = #selector(resetField(_:))
        control.toolTip = help
        let top = NSStackView(views: [name, control, origin, reset])
        top.orientation = .horizontal; top.spacing = 8; top.alignment = .centerY
        let detail = label(help, secondary: true)
        detail.lineBreakMode = .byWordWrapping; detail.maximumNumberOfLines = 2
        detail.preferredMaxLayoutWidth = 380
        let spacer = NSView(); spacer.widthAnchor.constraint(equalToConstant: 128).isActive = true
        let bottom = NSStackView(views: [spacer, detail])
        bottom.orientation = .horizontal; bottom.spacing = 0
        let both = NSStackView(views: [top, bottom])
        both.orientation = .vertical; both.alignment = .leading; both.spacing = 2
        return both
    }
    func applicationDidFinishLaunching(_ note: Notification) {
        let menu = NSMenu()
        let appItem = NSMenuItem(); menu.addItem(appItem)
        let appMenu = NSMenu(); appItem.submenu = appMenu
        appMenu.addItem(withTitle: "Close Settings", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        appMenu.addItem(withTitle: "Quit Gliff", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        // An Edit menu, so Cut/Copy/Paste work in the server command field.
        let editItem = NSMenuItem(); menu.addItem(editItem)
        let editMenu = NSMenu(title: "Edit"); editItem.submenu = editMenu
        editMenu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        editMenu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        editMenu.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        NSApplication.shared.mainMenu = menu

        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 780, height: 420), styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        w.title = "Gliff Settings"; w.delegate = self
        w.contentMinSize = NSSize(width: 700, height: 360)
        let view = w.contentView!

        search.placeholderString = "Search machines"; search.delegate = self
        search.setAccessibilityLabel("Search machines")
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("scope")); column.title = "Machine"
        table.addTableColumn(column); table.headerView = nil
        table.delegate = self; table.dataSource = self; table.rowHeight = 24; table.style = .sourceList
        table.setAccessibilityLabel("Machines")
        let scroll = NSScrollView(); scroll.documentView = table; scroll.hasVerticalScroller = true
        scroll.translatesAutoresizingMaskIntoConstraints = false
        let sidebar = NSStackView(views: [search, scroll])
        sidebar.orientation = .vertical; sidebar.spacing = 8; sidebar.alignment = .leading
        sidebar.translatesAutoresizingMaskIntoConstraints = false

        heading.font = NSFont.systemFont(ofSize: 17, weight: .semibold)
        heading.lineBreakMode = .byTruncatingMiddle
        scopeNote.font = NSFont.systemFont(ofSize: 12); scopeNote.textColor = .secondaryLabelColor
        scopeNote.preferredMaxLayoutWidth = 480
        screen.addItems(withTitles: ["Mirror screen", "Private screen"])
        screen.target = self; screen.action = #selector(screenChanged)
        screen.setAccessibilityLabel("Screen")
        clipboardBox.target = self; clipboardBox.action = #selector(clipboardChanged)
        serverBin.target = self; serverBin.action = #selector(serverChanged)
        serverBin.cell?.sendsActionOnEndEditing = true
        serverBin.setAccessibilityLabel("Server command")
        serverBin.widthAnchor.constraint(greaterThanOrEqualToConstant: 240).isActive = true
        screenReset.identifier = NSUserInterfaceItemIdentifier("screen")
        clipboardReset.identifier = NSUserInterfaceItemIdentifier("clipboard")
        serverReset.identifier = NSUserInterfaceItemIdentifier("server_bin")
        let display = row("Screen", screen, screenOrigin, screenReset, "Mirror shows the machine's screen; Private makes a separate Linux screen that follows this window.")
        let clip = row("Clipboard", clipboardBox, clipboardOrigin, clipboardReset, "Copy on one machine and paste on the other. Turn off for machines you do not trust.")
        let server = row("Server command", serverBin, serverOrigin, serverReset, "Starts the server over SSH, e.g. gliff-server --low-bandwidth. Blank uses the inherited command.")
        let rule = label("A screen or server command change reconnects the machine; the clipboard changes at once. Changes are saved as you make them.", secondary: true)
        rule.preferredMaxLayoutWidth = 480; rule.lineBreakMode = .byWordWrapping; rule.maximumNumberOfLines = 3
        pathLabel.font = NSFont.systemFont(ofSize: 11); pathLabel.textColor = .tertiaryLabelColor
        pathLabel.preferredMaxLayoutWidth = 480
        errorLabel.font = NSFont.systemFont(ofSize: 12); errorLabel.textColor = .systemRed
        errorLabel.preferredMaxLayoutWidth = 480
        let form = NSStackView(views: [heading, scopeNote, display, clip, server, rule, errorLabel, NSView(), pathLabel])
        form.orientation = .vertical; form.alignment = .leading; form.spacing = 14
        form.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(sidebar); view.addSubview(form)
        NSLayoutConstraint.activate([
            sidebar.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 16),
            sidebar.topAnchor.constraint(equalTo: view.topAnchor, constant: 16),
            sidebar.bottomAnchor.constraint(equalTo: view.bottomAnchor, constant: -16),
            sidebar.widthAnchor.constraint(equalToConstant: 220),
            search.widthAnchor.constraint(equalTo: sidebar.widthAnchor),
            scroll.widthAnchor.constraint(equalTo: sidebar.widthAnchor),
            form.leadingAnchor.constraint(equalTo: sidebar.trailingAnchor, constant: 20),
            form.trailingAnchor.constraint(lessThanOrEqualTo: view.trailingAnchor, constant: -20),
            form.topAnchor.constraint(equalTo: view.topAnchor, constant: 16),
            form.bottomAnchor.constraint(equalTo: view.bottomAnchor, constant: -16),
        ])
        window = w
        reloadScopes()
        w.center(); w.makeKeyAndOrderFront(nil)
        NSApplication.shared.activate(ignoringOtherApps: true)
        readCommands { [weak self] _, data in
            let cmd = json(data)
            DispatchQueue.main.async {
                guard let self = self else { return }
                switch cmd["type"] as? String ?? "" {
                case "settings_state":
                    self.state = cmd
                    let path = cmd["path"] as? String ?? ""
                    self.pathLabel.stringValue = path.isEmpty ? "Settings are not saved (no home folder)." : "Saved in \(path)"
                    self.reloadScopes()
                case "settings_error":
                    self.errorLabel.stringValue = cmd["message"] as? String ?? "That change was not saved."
                default: break
                }
            }
        }
    }

    // Data from settings_state.
    func layer(_ scope: String) -> [String: Any] {
        if scope.isEmpty { return state["defaults"] as? [String: Any] ?? [:] }
        let machines = state["machines"] as? [[String: Any]] ?? []
        return machines.first(where: { $0["name"] as? String == scope })?["settings"] as? [String: Any] ?? [:]
    }
    // What the selected scope inherits for a field, and from where.
    func inherited(_ field: String) -> (Any?, String) {
        if !selected.isEmpty, let v = layer("")[field] { return (v, "Default") }
        return ((state["builtin"] as? [String: Any])?[field], "Built-in")
    }
    // The value in force for the selected scope; `here` when this scope sets it.
    func effective(_ field: String) -> (value: Any?, origin: String, here: Bool) {
        if let v = layer(selected)[field] { return (v, selected.isEmpty ? "Set for all machines" : "Set for this machine", true) }
        let (v, origin) = inherited(field)
        return (v, origin, false)
    }

    func reloadScopes() {
        let query = search.stringValue.trimmingCharacters(in: .whitespaces).lowercased()
        let names = (state["machines"] as? [[String: Any]] ?? []).compactMap { $0["name"] as? String }
        scopes = [""] + names.filter { query.isEmpty || $0.lowercased().contains(query) }
        if !scopes.contains(selected) { selected = "" }
        updating = true
        table.reloadData()
        if let index = scopes.firstIndex(of: selected) { table.selectRowIndexes(IndexSet(integer: index), byExtendingSelection: false) }
        updating = false
        refreshForm()
    }
    func refreshForm() {
        updating = true
        heading.stringValue = selected.isEmpty ? "All Machines" : selected
        scopeNote.stringValue = selected.isEmpty
            ? "Used by every machine that does not set its own value."
            : "Only this machine. Reset hands an option back to All Machines."
        let s = effective("screen")
        screen.selectItem(at: s.value as? String == "private" ? 1 : 0)
        screenOrigin.stringValue = s.origin; screenReset.isHidden = !s.here
        let c = effective("clipboard")
        clipboardBox.state = c.value as? Bool == false ? .off : .on
        clipboardOrigin.stringValue = c.origin; clipboardReset.isHidden = !c.here
        let b = effective("server_bin")
        serverBin.placeholderString = inherited("server_bin").0 as? String ?? "gliff-server"
        // Do not replace text the user is typing.
        let editing = window?.firstResponder === serverBin.currentEditor()
        if !editing { serverBin.stringValue = b.here ? (b.value as? String ?? "") : "" }
        serverOrigin.stringValue = b.origin; serverReset.isHidden = !b.here
        updating = false
    }

    func edit(_ field: String, _ value: Any) {
        errorLabel.stringValue = ""
        event(["type": "settings_edit", "scope": selected.isEmpty ? "defaults" : "machine", "machine": selected, "field": field, "value": value])
    }
    @objc func screenChanged(_ sender: Any?) { if !updating { edit("screen", screen.indexOfSelectedItem == 1 ? "private" : "mirror") } }
    @objc func clipboardChanged(_ sender: Any?) { if !updating { edit("clipboard", clipboardBox.state == .on) } }
    @objc func serverChanged(_ sender: Any?) {
        guard !updating else { return }
        let text = serverBin.stringValue.trimmingCharacters(in: .whitespaces)
        let current = effective("server_bin")
        // Blank on a scope that sets nothing is no edit; blank otherwise clears.
        if text.isEmpty && !current.here { return }
        if current.here, current.value as? String == text { return }
        edit("server_bin", text.isEmpty ? NSNull() : text)
    }
    @objc func resetField(_ sender: NSButton) {
        guard let field = sender.identifier?.rawValue else { return }
        edit(field, NSNull())
    }

    func numberOfRows(in tableView: NSTableView) -> Int { scopes.count }
    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard scopes.indices.contains(row) else { return nil }
        let name = scopes[row]
        let cell = NSTableCellView()
        let text = NSTextField(labelWithString: name.isEmpty ? "All Machines" : name)
        text.lineBreakMode = .byTruncatingMiddle; text.translatesAutoresizingMaskIntoConstraints = false
        if name.isEmpty { text.font = NSFont.systemFont(ofSize: 13, weight: .semibold) }
        cell.textField = text; cell.addSubview(text)
        NSLayoutConstraint.activate([
            text.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 6),
            text.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -6),
            text.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
        ])
        return cell
    }
    func tableViewSelectionDidChange(_ notification: Notification) {
        guard !updating, scopes.indices.contains(table.selectedRow) else { return }
        // Commit a half-typed server command to the scope it was typed for.
        if window?.firstResponder === serverBin.currentEditor() { serverChanged(nil); window?.makeFirstResponder(nil) }
        selected = scopes[table.selectedRow]; errorLabel.stringValue = ""
        refreshForm()
    }
    func controlTextDidChange(_ note: Notification) {
        if (note.object as AnyObject?) === search { reloadScopes() }
    }
    func windowWillClose(_ note: Notification) {
        if window?.firstResponder === serverBin.currentEditor() { serverChanged(nil) }
        event(["type": "settings_close"])
    }
    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { true }
}

let mode = ProcessInfo.processInfo.environment["GLIFF_ASKPASS"] == "1" ? "askpass" : (CommandLine.arguments.dropFirst().first ?? "permissions")
let app = NSApplication.shared
if mode == "encode" {
    let args = Array(CommandLine.arguments.dropFirst(2))
    guard args.count == 4, let width = Int(args[0]), let height = Int(args[1]), let bitrate = Int(args[2]), let fps = Int(args[3]) else { fail("Invalid encoder configuration") }
    do {
        let encoder = try HardwareEncoder(width, height, bitrate, fps)
        event(["type": "encoder", "hardware": true, "name": "videotoolbox"])
        app.setActivationPolicy(.prohibited)
        readCommands(onClose: { encoder.close() }) { kind, data in
            guard kind == 4 else { fail("Unexpected encoder command") }
            encoder.encode(data)
        }
        withExtendedLifetime(encoder) { app.run() }
    } catch { fail(error.localizedDescription) }
} else if mode == "askpass" {
    app.setActivationPolicy(.accessory); app.activate(ignoringOtherApps: true)
    let prompt = CommandLine.arguments.dropFirst().joined(separator: " ")
    let alert = NSAlert(); alert.messageText = "SSH Authentication"; alert.informativeText = prompt
    if prompt.contains("yes/no") {
        alert.messageText = "Connect to this computer?"
        alert.addButton(withTitle: "Connect"); alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { exit(1) }
        print("yes")
    } else {
        let field = NSSecureTextField(frame: NSRect(x: 0, y: 0, width: 340, height: 26))
        alert.accessoryView = field
        alert.addButton(withTitle: "Continue"); alert.addButton(withTitle: "Cancel")
        alert.window.initialFirstResponder = field
        guard alert.runModal() == .alertFirstButtonReturn else { exit(1) }
        print(field.stringValue)
    }
    exit(0)
} else if mode == "choose" {
    app.setActivationPolicy(.regular)
    let picker = ComputerPicker(screenOverride: CommandLine.arguments.dropFirst(2).first); app.delegate = picker
    withExtendedLifetime(picker) { app.run() }
} else if mode == "settings" {
    app.setActivationPolicy(.regular)
    let settings = SettingsWindow(); app.delegate = settings
    withExtendedLifetime(settings) { app.run() }
} else if mode == "keyboard-layout" {
    guard let layout = keyboardLayout(CommandLine.arguments.dropFirst(2).first) else { fail("Cannot read the Mac Unicode keyboard layout") }
    event(layout)
} else if mode == "discover" {
    app.setActivationPolicy(.accessory)
    let nearby = NearbyComputers(); nearby.start()
    Timer.scheduledTimer(withTimeInterval: 8, repeats: false) { _ in
        if let failure = nearby.failure { event(["type": "error", "message": failure]) }
        else { event(["type": "hosts", "hosts": nearby.computers.map { $0.value }]) }
        nearby.stop(); app.terminate(nil)
    }
    withExtendedLifetime(nearby) { app.run() }
} else if mode == "app" {
    app.setActivationPolicy(.regular)
    let delegate = App(); app.delegate = delegate
    withExtendedLifetime(delegate) { app.run() }
} else if mode == "capture" {
    app.setActivationPolicy(.accessory)
    let args = Array(CommandLine.arguments.dropFirst(2))
    let capture = Capture()
    readCommands(onClose: { capture.input.handle(["type": "release"]) }) { _, data in
        let cmd = json(data)
        if clipboard.receive(cmd) { return }
        DispatchQueue.main.async {
            if cmd["type"] as? String == "resize" { capture.resize(cmd["width"] as? Int ?? 1920, cmd["height"] as? Int ?? 1200) }
            else { capture.input.handle(cmd); clipboard.command(cmd) }
        }
    }
    Task { await capture.start(args.first ?? "auto", Int(args.count > 1 ? args[1] : "1920") ?? 1920, Int(args.count > 2 ? args[2] : "1200") ?? 1200, Int(args.count > 3 ? args[3] : "30") ?? 30) }
    app.run()
    capture.input.handle(["type": "release"])
} else if mode == "outputs" {
    var displays = [CGDirectDisplayID](repeating: 0, count: 32), count: UInt32 = 0
    CGGetActiveDisplayList(32, &displays, &count)
    for id in displays.prefix(Int(count)) {
        let b = CGDisplayBounds(id)
        print("display-\(id)\t\(CGDisplayPixelsWide(id))x\(CGDisplayPixelsHigh(id))\t\(Int(b.width))x\(Int(b.height)) logical\(id == CGMainDisplayID() ? "\tmain" : "")")
    }
} else {
    if CommandLine.arguments.contains("--request") {
        _ = CGRequestScreenCaptureAccess()
        _ = AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary)
    }
    print("Screen Recording: \(CGPreflightScreenCaptureAccess() ? "granted" : "required")")
    print("Accessibility: \(AXIsProcessTrusted() ? "granted" : "required")")
    if !CGPreflightScreenCaptureAccess() || !AXIsProcessTrusted() { exit(1) }
}
