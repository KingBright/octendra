// Original Octendra implementation using Apple's public Virtualization API.
// The guest owns its disk, applications, input devices and login state. No host
// folders, clipboard, microphone or audio device are attached implicitly.
import AppKit
import Foundation
import Virtualization
import Darwin

private let gib: UInt64 = 1 << 30
private struct VMFailure: Error { let code: String }
private struct VMProfile: Codable {
    var format = 1
    var id: String
    var cpuCount: Int
    var memoryBytes: UInt64
    var diskBytes: UInt64
    var macAddress: String
    var hardwareModel: Data
    var machineIdentifier: Data
    var restoreURL: URL
    var restoreBuild: String
    var installed = false
    var setupRequested = false
}

private func privateWrite(_ data: Data, to url: URL) throws {
    try data.write(to: url, options: .atomic)
    try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
}
private func emit(_ value: [String: Any]) {
    if let data = try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]) {
        FileHandle.standardOutput.write(data)
        FileHandle.standardOutput.write(Data([10]))
    }
}

/// URLSession owns its temporary partial file. Resume data stays in this one
/// private workspace, and retries retain the original restore URL and VM ID.
private final class RestoreDownload: NSObject, URLSessionDownloadDelegate {
    let destination: URL
    let resume: URL
    var continuation: CheckedContinuation<URL, Error>?
    var session: URLSession?
    init(root: URL) {
        destination = root.appendingPathComponent("restore.ipsw")
        resume = root.appendingPathComponent("restore-resume.data")
    }
    func fetch(_ url: URL) async throws -> URL {
        try await withCheckedThrowingContinuation { continuation in
            self.continuation = continuation
            let configuration = URLSessionConfiguration.default
            configuration.waitsForConnectivity = true
            let session = URLSession(configuration: configuration, delegate: self, delegateQueue: nil)
            self.session = session
            let task: URLSessionDownloadTask
            if let data = try? Data(contentsOf: resume) {
                task = session.downloadTask(withResumeData: data)
            } else {
                task = session.downloadTask(with: url)
            }
            task.resume()
        }
    }
    func urlSession(_ session: URLSession, downloadTask: URLSessionDownloadTask, didFinishDownloadingTo location: URL) {
        do {
            guard let response = downloadTask.response as? HTTPURLResponse,
                  response.statusCode == 200 || response.statusCode == 206 else {
                throw VMFailure(code: "restore_http_response_failed")
            }
            guard !FileManager.default.fileExists(atPath: destination.path) else {
                throw VMFailure(code: "restore_destination_already_exists")
            }
            try FileManager.default.moveItem(at: location, to: destination)
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: destination.path)
            try? FileManager.default.removeItem(at: resume)
            continuation?.resume(returning: destination)
            continuation = nil
        } catch {
            continuation?.resume(throwing: error)
            continuation = nil
        }
    }
    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        if let error = error as NSError? {
            if let data = error.userInfo[NSURLSessionDownloadTaskResumeData] as? Data {
                try? privateWrite(data, to: resume)
            }
            continuation?.resume(throwing: error)
            continuation = nil
        }
        session.finishTasksAndInvalidate()
    }
}

@available(macOS 13, *)
@MainActor private final class VMHost: NSObject, @preconcurrency VZVirtualMachineDelegate, NSWindowDelegate {
    static var active: VMHost?
    let root: URL
    var profile: VMProfile?
    var machine: VZVirtualMachine?
    var activeConfiguration: VZVirtualMachineConfiguration?
    var window: NSWindow?
    var signals: [DispatchSourceSignal] = []
    var suspending = false
    var phase = "not_started"
    init(root: URL) { self.root = root }
    func path(_ name: String) -> URL { root.appendingPathComponent(name) }
    func load() throws -> VMProfile {
        let value = try JSONDecoder().decode(VMProfile.self, from: Data(contentsOf: path("workspace.json")))
        guard value.format == 1, UUID(uuidString: value.id) != nil else {
            throw VMFailure(code: "invalid_workspace_identity")
        }
        profile = value
        return value
    }
    func save(_ value: VMProfile) throws {
        try privateWrite(JSONEncoder().encode(value), to: path("workspace.json"))
        profile = value
    }
    func state(_ phase: String) {
        self.phase = phase
        let value: [String: Any] = ["state": phase, "workspace_id": profile?.id ?? "",
            "pid": ProcessInfo.processInfo.processIdentifier, "isolation": "full_vm",
            "graphical_acceptance": "not_verified"]
        if let data = try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]) {
            try? privateWrite(data, to: path("runtime.json"))
        }
        emit(value)
    }
    func suspend() async throws {
        guard let vm = machine else { throw VMFailure(code: "guest_not_running") }
        guard #available(macOS 14, *) else {
            try vm.requestStop()
            state("guest_stop_requested")
            return
        }
        guard !suspending else { return }
        suspending = true
        defer { suspending = false }
        guard let configuration = activeConfiguration else { throw VMFailure(code: "guest_configuration_unavailable") }
        try configuration.validateSaveRestoreSupport()
        let temporary = path("suspend-in-progress.bin")
        let saved = path("suspended-state.bin")
        guard !FileManager.default.fileExists(atPath: saved.path) else {
            throw VMFailure(code: "saved_guest_state_already_exists")
        }
        // An interrupted save is never treated as a completed suspension. This
        // single owned temporary file may be replaced only by the locked owner.
        if FileManager.default.fileExists(atPath: temporary.path) {
            try FileManager.default.removeItem(at: temporary)
        }
        state("suspending_guest")
        try await vm.pause()
        do {
            try await vm.saveMachineStateTo(url: temporary)
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: temporary.path)
            try FileManager.default.moveItem(at: temporary, to: saved)
        } catch {
            if vm.state == .paused { try? await vm.resume() }
            throw error
        }
        state("guest_suspended")
        exit(0)
    }
    func prepare() async throws {
        guard VZVirtualMachine.isSupported else { throw VMFailure(code: "virtualization_not_supported") }
        if FileManager.default.fileExists(atPath: path("workspace.json").path) {
            let value = try load()
            if value.installed { _ = try configuration(value); state("installed"); return }
        } else {
            let entries = try FileManager.default.contentsOfDirectory(atPath: root.path)
            guard entries.allSatisfy({ $0 == ".vm.lock" }) else {
                throw VMFailure(code: "workspace_directory_not_empty")
            }
            let free = (try FileManager.default.attributesOfFileSystem(forPath: root.path)[.systemFreeSize] as? NSNumber)?.uint64Value ?? 0
            guard free >= 48 * gib else { throw VMFailure(code: "guest_disk_capacity_unavailable") }
            let image = try await VZMacOSRestoreImage.latestSupported
            guard let requirements = image.mostFeaturefulSupportedConfiguration else {
                throw VMFailure(code: "restore_image_not_supported_on_host")
            }
            let value = VMProfile(id: UUID().uuidString.lowercased(),
                cpuCount: max(4, requirements.minimumSupportedCPUCount),
                memoryBytes: max(8 * gib, requirements.minimumSupportedMemorySize),
                diskBytes: 48 * gib, macAddress: VZMACAddress.randomLocallyAdministered().string,
                hardwareModel: requirements.hardwareModel.dataRepresentation,
                machineIdentifier: VZMacMachineIdentifier().dataRepresentation,
                restoreURL: image.url, restoreBuild: image.buildVersion)
            try save(value)
        }
        guard var value = profile else { throw VMFailure(code: "workspace_profile_missing") }
        state("downloading_restore_image")
        let media = path("restore.ipsw")
        if !FileManager.default.fileExists(atPath: media.path) {
            while true {
                do { _ = try await RestoreDownload(root: root).fetch(value.restoreURL); break }
                catch {
                    let error = error as NSError
                    guard error.domain == NSURLErrorDomain,
                          [NSURLErrorNetworkConnectionLost, NSURLErrorNotConnectedToInternet,
                           NSURLErrorTimedOut, NSURLErrorCannotFindHost, NSURLErrorCannotConnectToHost].contains(error.code) else { throw error }
                    state("waiting_for_restore_network")
                    try await Task.sleep(nanoseconds: 5_000_000_000)
                }
            }
        }
        let image = try await VZMacOSRestoreImage.image(from: media)
        guard image.isSupported, image.buildVersion == value.restoreBuild else {
            throw VMFailure(code: "restore_image_identity_mismatch")
        }
        let disk = path("disk.img")
        if !FileManager.default.fileExists(atPath: disk.path) {
            guard FileManager.default.createFile(atPath: disk.path, contents: nil, attributes: [.posixPermissions: 0o600]) else {
                throw VMFailure(code: "disk_creation_failed")
            }
            let file = try FileHandle(forWritingTo: disk)
            try file.truncate(atOffset: value.diskBytes)
            try file.close()
        }
        let vm = VZVirtualMachine(configuration: try configuration(value), queue: .main)
        machine = vm
        state("installing_guest_os")
        let installer = VZMacOSInstaller(virtualMachine: vm, restoringFromImageAt: media)
        try await installer.install()
        value.installed = true
        try save(value)
        // Only this workspace's verified installation media is temporary. The
        // persistent disk, machine identity and all user data remain intact.
        try FileManager.default.removeItem(at: media)
        state("installed")
    }
    func configuration(_ value: VMProfile) throws -> VZVirtualMachineConfiguration {
        guard let model = VZMacHardwareModel(dataRepresentation: value.hardwareModel), model.isSupported,
              let identifier = VZMacMachineIdentifier(dataRepresentation: value.machineIdentifier) else {
            throw VMFailure(code: "machine_identity_unavailable")
        }
        let platform = VZMacPlatformConfiguration()
        platform.hardwareModel = model
        platform.machineIdentifier = identifier
        let auxiliary = path("auxiliary-storage.bin")
        if value.installed && !FileManager.default.fileExists(atPath: auxiliary.path) {
            throw VMFailure(code: "persistent_auxiliary_storage_missing")
        }
        platform.auxiliaryStorage = FileManager.default.fileExists(atPath: auxiliary.path)
            ? VZMacAuxiliaryStorage(url: auxiliary)
            : try VZMacAuxiliaryStorage(creatingStorageAt: auxiliary, hardwareModel: model, options: [])
        let config = VZVirtualMachineConfiguration()
        config.platform = platform
        config.bootLoader = VZMacOSBootLoader()
        config.cpuCount = value.cpuCount
        config.memorySize = value.memoryBytes
        let disk = VZVirtioBlockDeviceConfiguration(attachment: try VZDiskImageStorageDeviceAttachment(url: path("disk.img"), readOnly: false))
        config.storageDevices = [disk]
        let graphics = VZMacGraphicsDeviceConfiguration()
        graphics.displays = [VZMacGraphicsDisplayConfiguration(widthInPixels: 1440, heightInPixels: 900, pixelsPerInch: 80)]
        config.graphicsDevices = [graphics]
        config.keyboards = [VZUSBKeyboardConfiguration()]
        config.pointingDevices = [VZUSBScreenCoordinatePointingDeviceConfiguration()]
        let network = VZVirtioNetworkDeviceConfiguration()
        guard let address = VZMACAddress(string: value.macAddress) else { throw VMFailure(code: "invalid_network_identity") }
        network.macAddress = address
        network.attachment = VZNATNetworkDeviceAttachment()
        config.networkDevices = [network]
        config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
        try config.validate()
        activeConfiguration = config
        return config
    }
    func run() async throws {
        try await prepare()
        guard var value = profile else { throw VMFailure(code: "workspace_profile_missing") }
        let vm: VZVirtualMachine
        if let prepared = machine { vm = prepared }
        else {
            let config: VZVirtualMachineConfiguration
            if let prepared = activeConfiguration { config = prepared }
            else { config = try configuration(value) }
            vm = VZVirtualMachine(configuration: config, queue: .main)
        }
        vm.delegate = self
        machine = vm
        let options = VZMacOSVirtualMachineStartOptions()
        if !value.setupRequested {
            guard #available(macOS 27, *) else { throw VMFailure(code: "automatic_guest_setup_requires_macos_27") }
            let credentials = path("guest-credentials.json")
            let password: String
            if FileManager.default.fileExists(atPath: credentials.path) {
                let data = try JSONSerialization.jsonObject(with: Data(contentsOf: credentials)) as? [String: String]
                guard let saved = data?["password"], data?["username"] == "octendra" else {
                    throw VMFailure(code: "invalid_guest_credentials")
                }
                password = saved
            } else {
                password = UUID().uuidString.replacingOccurrences(of: "-", with: "") + UUID().uuidString.replacingOccurrences(of: "-", with: "")
                try privateWrite(JSONSerialization.data(withJSONObject: ["username": "octendra", "password": password]), to: credentials)
            }
            let provisioning = VZMacGuestProvisioningOptions()
            provisioning.fullName = "Octendra AI Workspace"
            provisioning.username = "octendra"
            provisioning.password = password
            provisioning.logsInAutomatically = true
            provisioning.enablesRemoteLogin = true
            try options.setGuestProvisioning(provisioning)
        }
        let saved = path("suspended-state.bin")
        if FileManager.default.fileExists(atPath: saved.path) {
            guard #available(macOS 14, *) else { throw VMFailure(code: "saved_guest_state_requires_macos_14") }
            state("restoring_guest")
            try await vm.restoreMachineStateFrom(url: saved)
            // Consume only after successful restoration, while still paused.
            // Once running, the disk can advance and this RAM image must never
            // be replayed against that newer disk after a host process crash.
            try FileManager.default.removeItem(at: saved)
            try await vm.resume()
        } else {
            state("starting_guest")
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
                vm.start(options: options) { error in
                    if let error { continuation.resume(throwing: error) } else { continuation.resume() }
                }
            }
        }
        value.setupRequested = true
        try save(value)
        let view = VZVirtualMachineView(frame: NSRect(x: 0, y: 0, width: 1440, height: 900))
        view.virtualMachine = vm
        view.capturesSystemKeys = true
        let window = NSWindow(contentRect: view.frame, styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
        window.title = "Octendra AI Workspace"
        window.contentView = view
        window.makeFirstResponder(view)
        window.isReleasedWhenClosed = false
        window.delegate = self
        window.orderBack(nil)
        self.window = window
        for number in [SIGTERM, SIGINT] {
            signal(number, SIG_IGN)
            let source = DispatchSource.makeSignalSource(signal: number, queue: .main)
            source.setEventHandler { [weak self] in
                guard let self else { return }
                Task { @MainActor in
                    do { try await self.suspend() }
                    catch {
                        self.state("guest_suspend_failed")
                        emit(["error": (error as? VMFailure)?.code ?? "guest_suspend_failed",
                              "message": String(error.localizedDescription.prefix(1024))])
                    }
                }
            }
            source.resume(); signals.append(source)
        }
        state("guest_started")
    }
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        sender.orderOut(nil)
        return false
    }
    func guestDidStop(_ virtualMachine: VZVirtualMachine) { state("guest_stopped"); exit(0) }
    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
        state("guest_runtime_failed"); exit(1)
    }
}

@_cdecl("octendra_macos_vm_main")
public func octendraMacOSVMMain(_ operation: UnsafePointer<CChar>, _ directory: UnsafePointer<CChar>) -> Int32 {
    let command = String(cString: operation)
    let root = URL(fileURLWithPath: String(cString: directory), isDirectory: true)
    if command == "probe" {
        let prepared = (try? Data(contentsOf: root.appendingPathComponent("workspace.json")))
            .flatMap { try? JSONDecoder().decode(VMProfile.self, from: $0) }?.installed ?? false
        emit(["virtualization_supported": VZVirtualMachine.isSupported,
              "automatic_guest_setup_supported": ProcessInfo.processInfo.isOperatingSystemAtLeast(OperatingSystemVersion(majorVersion: 27, minorVersion: 0, patchVersion: 0)),
              "persistent_workspace_prepared": prepared])
        return 0
    }
    if command == "status" {
        do {
            let profile = try JSONDecoder().decode(VMProfile.self, from: Data(contentsOf: root.appendingPathComponent("workspace.json")))
            let value = (try? Data(contentsOf: root.appendingPathComponent("runtime.json")))
                .flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] } ?? [:]
            emit(["workspace_id": profile.id, "installed": profile.installed,
                  "recorded_runtime": value, "runtime_observation": "record_only_not_live_acceptance"])
            return 0
        } catch { emit(["error": "workspace_state_unavailable"]); return 1 }
    }
    guard Thread.isMainThread else { emit(["error": "virtualization_requires_main_thread"]); return 1 }
    let application = NSApplication.shared
    application.setActivationPolicy(.accessory)
    Task { @MainActor in
        do {
            let host = VMHost(root: root)
            VMHost.active = host
            if command == "prepare" {
                try await host.prepare(); exit(0)
            } else if command == "run" {
                try await host.run()
            } else { throw VMFailure(code: "unknown_vm_operation") }
        } catch {
            let native = error as NSError
            var message = String(native.localizedDescription.prefix(1024))
            if let data = try? Data(contentsOf: root.appendingPathComponent("guest-credentials.json")),
               let values = try? JSONSerialization.jsonObject(with: data) as? [String: String],
               let password = values["password"], !password.isEmpty {
                message = message.replacingOccurrences(of: password, with: "[redacted]")
            }
            emit(["error": (error as? VMFailure)?.code ?? "virtualization_operation_failed",
                  "phase": VMHost.active?.phase ?? "not_started",
                  "native_domain": native.domain, "native_code": native.code, "message": message])
            exit(1)
        }
    }
    application.run()
    return 0
}
