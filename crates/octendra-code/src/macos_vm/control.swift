// Private guest-to-host transport. Events go only to this VM's own view;
// no global event posting, window activation or host pasteboard writes.
import AppKit
import Foundation
import ScreenCaptureKit
import Security
@preconcurrency import Virtualization
import Darwin

struct VMInput: Decodable {
    let kind: String
    var usage: UInt16?
    var x: Double?
    var y: Double?
    var button: String?
    var horizontal: Int16?
    var vertical: Int16?
}
struct VMControlRequest: Decodable {
    let workspace_id: String
    let operation: String
    var generation: String?
    var revision: UInt64?
    var size: [Int]?
    var actions: [VMInput]?
}
private func controlTrace(_ event: String) {
    if ProcessInfo.processInfo.environment["OCTENDRA_VM_CONTROL_TRACE"] == "1" {
        emit(["event":"guest_control_\(event)"])
    }
}
private func waitSocket(_ fd: Int32, _ events: Int16) throws {
    var item = pollfd(fd: fd, events: events, revents: 0)
    while true {
        let ready = poll(&item, 1, 10_000)
        if ready < 0 && errno == EINTR { continue }
        guard ready > 0, item.revents & events != 0 else { throw VMFailure(code: "guest_control_io_timeout") }
        return
    }
}
private func sendReply(_ data: Data, _ fd: Int32) throws {
    try data.withUnsafeBytes { bytes in
        var offset = 0
        while offset < bytes.count {
            try waitSocket(fd, Int16(POLLOUT))
            let count = Darwin.write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
            if count < 0 && [EINTR, EAGAIN].contains(errno) { continue }
            guard count > 0 else { throw VMFailure(code: "guest_control_write_failed") }
            offset += count
        }
    }
}

private func keyCode(_ usage: UInt16) -> UInt16? {
    let letters: [UInt16] = [0,11,8,2,14,3,5,4,34,38,40,37,46,45,31,35,12,15,1,17,32,9,13,7,16,6]
    let digits: [UInt16] = [18,19,20,21,23,22,26,28,25,29]
    if (4...29).contains(usage) { return letters[Int(usage - 4)] }
    if (30...39).contains(usage) { return digits[Int(usage - 30)] }
    return [40:36,41:53,42:51,43:48,44:49,45:27,46:24,47:33,48:30,49:42,
        51:41,52:39,53:50,54:43,55:47,56:44,79:124,80:123,81:125,82:126,
        224:59,225:56,226:58,227:55,228:62,229:60,230:61,231:54][usage]
}
private func modifier(_ usage: UInt16) -> NSEvent.ModifierFlags? {
    switch usage {
    case 224, 228: return .control
    case 225, 229: return .shift
    case 226, 230: return .option
    case 227, 231: return .command
    default: return nil
    }
}

@available(macOS 13, *)
extension VMHost: @preconcurrency VZVirtioSocketListenerDelegate {
    func startControl(_ vm: VZVirtualMachine) throws {
        guard let device = vm.socketDevices.first as? VZVirtioSocketDevice else {
            throw VMFailure(code: "guest_control_socket_unavailable")
        }
        let listener = VZVirtioSocketListener()
        listener.delegate = self
        device.setSocketListener(listener, forPort: 18789)
        controlListener = listener
    }
    func listener(_ listener: VZVirtioSocketListener, shouldAcceptNewConnection connection: VZVirtioSocketConnection,
                  from socketDevice: VZVirtioSocketDevice) -> Bool {
        // Bound retained descriptors and incomplete reads from this guest. This
        // is transport capacity, independent of the Agent's task/think lifetime.
        guard controlConnections < 8 else { return false }
        controlConnections += 1
        let fd = connection.fileDescriptor
        controlTrace("connected")
        var timeout = timeval(tv_sec: 10, tv_usec: 0)
        var one: Int32 = 1
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        _ = setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                var bytes = Data()
                while !bytes.contains(10) {
                    try waitSocket(fd, Int16(POLLIN))
                    var chunk = [UInt8](repeating: 0, count: 4096)
                    let count = Darwin.read(fd, &chunk, chunk.count)
                    if count < 0 && [EINTR, EAGAIN].contains(errno) { continue }
                    guard count > 0 else { throw VMFailure(code: "guest_control_request_incomplete") }
                    bytes.append(contentsOf: chunk.prefix(count))
                    guard bytes.count <= 64 * 1024 else { throw VMFailure(code: "guest_control_request_too_large") }
                }
                let request = try JSONDecoder().decode(VMControlRequest.self, from: bytes.prefix { $0 != 10 })
                controlTrace("request_received")
                Task { @MainActor in
                    let result = await self.control(request)
                    DispatchQueue.global(qos: .userInitiated).async {
                        defer { Task { @MainActor in connection.close(); self.controlConnections -= 1 } }
                        if var data = try? JSONSerialization.data(withJSONObject: result, options: [.sortedKeys]) {
                            data.append(10)
                            do { try sendReply(data, fd); controlTrace("reply_sent") }
                            catch { emit(["error":"guest_control_write_failed", "native_errno":errno]) }
                        }
                    }
                }
            } catch {
                emit(["error":(error as? VMFailure)?.code ?? "guest_control_read_failed", "native_errno":errno])
                Task { @MainActor in connection.close(); self.controlConnections -= 1 }
            }
        }
        return true
    }

    func control(_ request: VMControlRequest) async -> [String: Any] {
        guard request.workspace_id == profile?.id else {
            return ["error":"ui_execution_context_mismatch", "input_sent":false]
        }
        guard machine?.state == .running, !suspending, !controlBusy else {
            return ["error":"ui_workspace_not_ready", "input_sent":false]
        }
        controlBusy = true
        defer { controlBusy = false }
        var sent = false
        do {
            if request.operation == "observe" {
                controlTrace("capture_started")
                var result = try await captureControl()
                controlTrace("capture_completed")
                result["workspace_id"] = profile!.id
                result["generation"] = controlGeneration
                result["revision"] = controlRevision
                return result
            }
            guard request.operation == "input", request.generation == controlGeneration,
                  request.revision == controlRevision, request.size == controlSize,
                  let view = controlView, view.bounds.size == controlBounds,
                  let actions = request.actions, actions.count <= 128 else {
                throw VMFailure(code: "stale_observation")
            }
            try validateInput(actions)
            let foreground = NSWorkspace.shared.frontmostApplication?.processIdentifier
            let cursor = NSEvent.mouseLocation
            let clipboard = NSPasteboard.general.changeCount
            controlRevision += 1
            sent = !actions.isEmpty
            try await sendInput(actions, to: view)
            return ["state":"accepted", "input_sent":sent, "accepted_actions":actions.count,
                "effect_confirmed":false, "generation":controlGeneration, "revision":controlRevision,
                "host_impact":["foreground_unchanged":foreground == NSWorkspace.shared.frontmostApplication?.processIdentifier,
                    "cursor_unchanged":cursor == NSEvent.mouseLocation,
                    "clipboard_unchanged":clipboard == NSPasteboard.general.changeCount]]
        } catch {
            return ["error":(error as? VMFailure)?.code ?? "guest_control_failed",
                "message":String(error.localizedDescription.prefix(1024)),
                "input_sent":sent, "automatic_replay_safe":!sent]
        }
    }

    func captureControl() async throws -> [String: Any] {
        guard #available(macOS 14.4, *), let window, let view = controlView else {
            throw VMFailure(code: "guest_capture_requires_macos_14_4")
        }
        if !window.isVisible { window.orderBack(nil) }
        // Public API returns only content this process can capture without TCC.
        // Select our own exact console; never enumerate/capture other apps.
        let content = try await SCShareableContent.currentProcess
        controlTrace("own_content_resolved")
        guard let target = content.windows.first(where: {
            $0.windowID == CGWindowID(window.windowNumber)
                && $0.owningApplication?.processID == getpid()
        }) else { throw VMFailure(code: "guest_console_unavailable") }
        let scale = min(1, 1280 / view.bounds.width)
        let configuration = SCStreamConfiguration()
        configuration.width = max(1, Int((window.frame.width * scale).rounded()))
        configuration.height = max(1, Int((window.frame.height * scale).rounded()))
        configuration.showsCursor = false
        configuration.ignoreShadowsSingleWindow = true
        let full = try await SCScreenshotManager.captureImage(contentFilter: SCContentFilter(desktopIndependentWindow: target),
                                                             configuration: configuration)
        let width = min(full.width, Int((view.bounds.width * scale).rounded()))
        let height = min(full.height, Int((view.bounds.height * scale).rounded()))
        let top = max(0, full.height - height)
        guard width > 0, height > 0, width <= 1280, height <= 2048,
              let image = full.cropping(to: CGRect(x: 0, y: top, width: width, height: height)) else {
            throw VMFailure(code: "guest_frame_geometry_unavailable")
        }
        var pixels = Data(count: width * height * 4)
        let drawn = pixels.withUnsafeMutableBytes { bytes -> Bool in
            guard let context = CGContext(data: bytes.baseAddress, width: width, height: height,
                bitsPerComponent: 8, bytesPerRow: width * 4, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                bitmapInfo: CGBitmapInfo.byteOrder32Big.rawValue | CGImageAlphaInfo.premultipliedLast.rawValue) else { return false }
            context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
            return true
        }
        guard drawn else { throw VMFailure(code: "guest_frame_conversion_failed") }
        if controlBounds != view.bounds.size || controlSize != [width, height] {
            controlRevision += 1
        }
        controlBounds = view.bounds.size
        controlSize = [width, height]
        return ["size":controlSize, "encoding":"rgba8_srgb", "data":pixels.base64EncodedString()]
    }

    func validateInput(_ actions: [VMInput]) throws {
        var keys = Set<UInt16>(), buttons = Set<String>()
        for action in actions {
            var valid = false
            switch action.kind {
            case "pointer_move":
                if let x = action.x, let y = action.y {
                    valid = x.isFinite && y.isFinite && x >= 0 && y >= 0
                        && x < Double(controlSize[0]) && y < Double(controlSize[1])
                }
            case "key_down":
                if let usage = action.usage, keyCode(usage) != nil { valid = keys.insert(usage).inserted }
            case "key_up":
                if let usage = action.usage { valid = keys.remove(usage) != nil }
            case "button_down":
                if let button = action.button, ["left","right","middle"].contains(button) { valid = buttons.insert(button).inserted }
            case "button_up":
                if let button = action.button { valid = buttons.remove(button) != nil }
            case "wheel": valid = action.horizontal != nil && action.vertical != nil
            default: break
            }
            guard valid else { throw VMFailure(code: "invalid_actions") }
        }
        guard keys.isEmpty, buttons.isEmpty else { throw VMFailure(code: "unbalanced_actions") }
    }

    func sendInput(_ actions: [VMInput], to view: VZVirtualMachineView) async throws {
        var flags: NSEvent.ModifierFlags = []
        var location = NSPoint(x: view.bounds.midX, y: view.bounds.midY)
        var held: String?
        for action in actions {
            switch action.kind {
            case "key_down", "key_up":
                let down = action.kind == "key_down", usage = action.usage!
                let type: NSEvent.EventType = down ? .keyDown : .keyUp
                if let flag = modifier(usage) {
                    if down { flags.insert(flag) } else { flags.remove(flag) }
                }
                guard let event = NSEvent.keyEvent(with: type, location: .zero, modifierFlags: flags,
                    timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window!.windowNumber,
                    context: nil, characters: "", charactersIgnoringModifiers: "", isARepeat: false, keyCode: keyCode(usage)!) else {
                    throw VMFailure(code: "guest_keyboard_event_failed")
                }
                // VZ forwards explicit keyDown/keyUp to its USB keyboard.
                // NSResponder.flagsChanged alone does not deliver modifiers to
                // a background VM, although ordinary keys still arrive.
                if down { view.keyDown(with: event) } else { view.keyUp(with: event) }
            case "pointer_move", "button_down", "button_up":
                var type: NSEvent.EventType = .mouseMoved
                if action.kind == "pointer_move" {
                    let x = action.x! / Double(max(1, controlSize[0] - 1)) * (view.bounds.width - 1)
                    let y = action.y! / Double(max(1, controlSize[1] - 1)) * (view.bounds.height - 1)
                    location = view.convert(NSPoint(x: x, y: view.isFlipped ? y : view.bounds.height - 1 - y), to: nil)
                    if let held { type = held == "left" ? .leftMouseDragged : held == "right" ? .rightMouseDragged : .otherMouseDragged }
                } else {
                    let down = action.kind == "button_down", button = action.button!
                    type = button == "left" ? (down ? .leftMouseDown : .leftMouseUp)
                        : button == "right" ? (down ? .rightMouseDown : .rightMouseUp) : (down ? .otherMouseDown : .otherMouseUp)
                    held = down ? button : nil
                }
                guard let event = NSEvent.mouseEvent(with: type, location: location, modifierFlags: flags,
                    timestamp: ProcessInfo.processInfo.systemUptime, windowNumber: window!.windowNumber,
                    context: nil, eventNumber: 0, clickCount: action.kind == "pointer_move" ? 0 : 1, pressure: held == nil ? 0 : 1) else {
                    throw VMFailure(code: "guest_pointer_event_failed")
                }
                switch type {
                case .leftMouseDown: view.mouseDown(with: event)
                case .leftMouseUp: view.mouseUp(with: event)
                case .rightMouseDown: view.rightMouseDown(with: event)
                case .rightMouseUp: view.rightMouseUp(with: event)
                case .otherMouseDown: view.otherMouseDown(with: event)
                case .otherMouseUp: view.otherMouseUp(with: event)
                case .leftMouseDragged: view.mouseDragged(with: event)
                case .rightMouseDragged: view.rightMouseDragged(with: event)
                case .otherMouseDragged: view.otherMouseDragged(with: event)
                default: view.mouseMoved(with: event)
                }
            case "wheel":
                guard let raw = CGEvent(scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 2,
                    wheel1: Int32(action.vertical!), wheel2: Int32(action.horizontal!), wheel3: 0),
                    let event = NSEvent(cgEvent: raw) else { throw VMFailure(code: "guest_scroll_event_failed") }
                view.scrollWheel(with: event)
            default: throw VMFailure(code: "invalid_actions")
            }
            // Virtual hardware delivery settles without touching the host event queue.
            try await Task.sleep(nanoseconds: 5_000_000)
        }
    }
}

@_cdecl("octendra_macos_guest_copy")
public func octendraMacOSGuestCopy(_ text: UnsafePointer<CChar>) -> Int32 {
    // SSH and GUI audit sessions can have different pasteboards, even for the
    // same uid. The normal installer runs the Agent in gui/<uid>. Refuse a
    // remote-shell copy before mutating the wrong session's pasteboard.
    var attributes: SessionAttributeBits = []
    let status = SessionGetInfo(callerSecuritySession, nil, &attributes)
    if status != errSecSuccess { return status }
    if attributes.rawValue & 0x0010 == 0 { return 2 } // sessionHasGraphicAccess
    let pasteboard = NSPasteboard.general
    pasteboard.clearContents()
    return pasteboard.setString(String(cString: text), forType: .string) ? 0 : 1
}
