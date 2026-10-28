import Darwin
import ExtensionFoundation
import FSKit
import Foundation

@main
struct Ext4FSExtension: UnaryFileSystemExtension {
    let fileSystem = Ext4FileSystem()
}

final class Ext4FileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations, FSManageableResourceMaintenanceOperations {
    private let stateLock = NSLock()
    private var maintenanceResource: FSBlockDeviceResource?
    private var loadedVolume: Ext4Volume?

    private struct ProbeFailure: Error {
        let errno: Int32
    }

    func didFinishLoading() {
        ext4Logger.notice("Ext4 FSKit module loaded")
    }

    func probeResource(resource: FSResource, replyHandler reply: @escaping (FSProbeResult?, (any Error)?) -> Void) {
        guard let blockResource = resource as? FSBlockDeviceResource else {
            reply(.notRecognized, nil)
            return
        }
        let device = blockResource.bsdName
        switch probe(blockResource) {
        case .failure(let failure) where failure.errno == Int32(EINVAL):
            ext4Logger.info("probe \(device, privacy: .public): not ext2/3/4")
            reply(.notRecognized, nil)
        case .failure(let failure):
            ext4Logger.error("probe \(device, privacy: .public) failed: errno \(failure.errno, privacy: .public)")
            reply(nil, posixError(failure.errno))
        case .success(let info):
            let label = stringFromCBytes(info.label)
            let containerID = FSContainerIdentifier(uuid: uuidFromCBytes(info.uuid))
            let result: FSProbeResult
            // Disk Arbitration only mounts "usable" results. Read-only
            // volumes are still usable; activate mounts them read-only.
            if info.readable {
                result = .usable(name: label, containerID: containerID)
            } else {
                result = .recognized(name: label, containerID: containerID)
            }
            ext4Logger.notice("probe \(device, privacy: .public): \(stringFromCBytes(info.flavor), privacy: .public) \"\(label, privacy: .public)\" readable=\(info.readable, privacy: .public) writable=\(info.writable, privacy: .public)")
            reply(result, nil)
        }
    }

    func loadResource(resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping (FSVolume?, (any Error)?) -> Void) {
        guard let blockResource = resource as? FSBlockDeviceResource else {
            reply(nil, posixError(Int32(EINVAL)))
            return
        }
        let forced = options.taskOptions.contains("-f")
        stateLock.lock()
        maintenanceResource = blockResource
        stateLock.unlock()

        let info: ext4fs_probe_info
        switch probe(blockResource) {
        case .failure(let failure):
            if forced {
                containerStatus = .notReady(status: posixError(Int32(EPROTONOSUPPORT)))
                reply(nil, nil)
            } else {
                reply(nil, posixError(failure.errno))
            }
            return
        case .success(let value):
            info = value
        }

        guard info.readable else {
            if forced {
                containerStatus = .notReady(status: posixError(Int32(EPROTONOSUPPORT)))
                reply(nil, nil)
            } else {
                let unusable = NSError(domain: FSKitErrorDomain, code: 4503)
                containerStatus = .blocked(status: unusable)
                reply(nil, unusable)
            }
            return
        }

        let label = stringFromCBytes(info.label)
        let volume = Ext4Volume(
            bridge: DeviceBridge(resource: blockResource),
            identifier: FSVolume.Identifier(uuid: uuidFromCBytes(info.uuid)),
            name: FSFileName(string: label.isEmpty ? "Untitled" : label),
            requestedReadOnly: options.taskOptions.contains("--rdonly") || !info.writable || !blockResource.isWritable
        )
        stateLock.lock()
        loadedVolume = volume
        stateLock.unlock()
        containerStatus = .ready
        reply(volume, nil)
    }

    func unloadResource(resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping ((any Error)?) -> Void) {
        stateLock.lock()
        let volume = loadedVolume
        loadedVolume = nil
        maintenanceResource = nil
        stateLock.unlock()

        guard let volume else {
            reply(nil)
            return
        }
        volume.finishUnmount { error in
            reply(error)
        }
    }

    func startCheck(task: FSTask, options: FSTaskOptions) throws -> Progress {
        let resource = try currentMaintenanceResource()
        if currentVolumeIsActive() { throw posixError(Int32(EBUSY)) }
        let progress = Progress(totalUnitCount: 1_000)
        let bridge = DeviceBridge(resource: resource)
        let taskOptions = Set(options.taskOptions)
        guard !(taskOptions.contains("-n") && taskOptions.contains("-y")) else {
            throw posixError(Int32(EINVAL))
        }
        var flags: UInt32 = UInt32(EXT4FS_CHECK_QUICK)
        if taskOptions.contains("-y") {
            flags = UInt32(EXT4FS_CHECK_REPAIR)
            if taskOptions.contains("-f") {
                flags |= UInt32(EXT4FS_CHECK_FORCE)
            }
        }
        ext4Logger.notice("check options: \(options.taskOptions, privacy: .public)")
        if taskOptions.contains("-q") {
            task.logMessage("Running quick ext4 consistency check")
        }
        DispatchQueue.global(qos: .userInitiated).async {
            var device = bridge.makeDevice()
            let context = Unmanaged.passUnretained(task).toOpaque()
            let progressContext = Unmanaged.passUnretained(progress).toOpaque()
            let result = withUnsafePointer(to: &device) { devicePointer in
                ext4fs_check(devicePointer, flags, Ext4FileSystem.checkMessage, context, Ext4FileSystem.updateProgress, progressContext)
            }
            progress.completedUnitCount = 1_000
            task.didComplete(error: result == 0 ? nil : posixError(result))
        }
        return progress
    }

    func startFormat(task: FSTask, options: FSTaskOptions) throws -> Progress {
        let resource = try currentMaintenanceResource()
        if currentVolumeIsActive() { throw posixError(Int32(EBUSY)) }
        let formatOptions = try MkfsOptions(taskOptions: options.taskOptions)
        guard resource.isWritable else { throw posixError(Int32(EROFS)) }
        let progress = Progress(totalUnitCount: 1_000)
        let bridge = DeviceBridge(resource: resource)
        DispatchQueue.global(qos: .userInitiated).async {
            var device = bridge.makeDevice()
            var ffiOptions = formatOptions.ffiOptions
            let result = formatOptions.withLabelPointer { labelPointer in
                ffiOptions.label = labelPointer
                return withUnsafePointer(to: &device) { devicePointer in
                    ext4fs_mkfs(devicePointer, &ffiOptions, Ext4FileSystem.updateProgress, Unmanaged.passUnretained(progress).toOpaque(), nil, 0)
                }
            }
            progress.completedUnitCount = 1_000
            task.didComplete(error: result == 0 ? nil : posixError(result))
        }
        return progress
    }

    private func currentMaintenanceResource() throws -> FSBlockDeviceResource {
        stateLock.lock()
        defer { stateLock.unlock() }
        guard let maintenanceResource else { throw posixError(Int32(ENODEV)) }
        return maintenanceResource
    }

    private func probe(_ resource: FSBlockDeviceResource) -> Result<ext4fs_probe_info, ProbeFailure> {
        let bridge = DeviceBridge(resource: resource)
        var device = bridge.makeDevice()
        var info = ext4fs_probe_info()
        var message = [CChar](repeating: 0, count: 256)
        let result = message.withUnsafeMutableBufferPointer { messageBuffer in
            withUnsafePointer(to: &device) { ext4fs_probe($0, &info, messageBuffer.baseAddress, messageBuffer.count) }
        }
        return result == 0 ? .success(info) : .failure(ProbeFailure(errno: result))
    }

    private func currentVolumeIsActive() -> Bool {
        stateLock.lock()
        defer { stateLock.unlock() }
        return loadedVolume?.isActive ?? false
    }

    private static let checkMessage: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void = { context, message in
        guard let context, let message else { return }
        let task = Unmanaged<FSTask>.fromOpaque(context).takeUnretainedValue()
        task.logMessage(String(cString: message))
    }

    private static let updateProgress: @convention(c) (UnsafeMutableRawPointer?, Double) -> Void = { context, fraction in
        guard let context else { return }
        let progress = Unmanaged<Progress>.fromOpaque(context).takeUnretainedValue()
        progress.completedUnitCount = Int64(max(0, min(1, fraction)) * 1_000)
    }
}

private struct MkfsOptions {
    private let label: [CChar]?
    let ffiOptions: ext4fs_mkfs_options

    init(taskOptions: [String]) throws {
        var label: String?
        var revision: UInt32 = 4
        var blockSize: UInt32 = 0
        var inodeSize: UInt32 = 0
        var inodeRatio: UInt32 = 0
        var reservedPercent: UInt32 = UInt32.max
        var iterator = taskOptions.makeIterator()
        while let option = iterator.next() {
            switch option {
            case "-L": label = try Self.value(after: option, from: &iterator)
            case "-b": blockSize = try Self.uint32(after: option, from: &iterator)
            case "-I": inodeSize = try Self.uint32(after: option, from: &iterator)
            case "-i": inodeRatio = try Self.uint32(after: option, from: &iterator)
            case "-m": reservedPercent = try Self.uint32(after: option, from: &iterator)
            case "-t":
                switch try Self.value(after: option, from: &iterator) {
                case "ext2": revision = 2
                case "ext3": revision = 3
                case "ext4": revision = 4
                default: throw posixError(Int32(EINVAL))
                }
            default:
                throw posixError(Int32(EINVAL))
            }
        }
        if let label, label.utf8.count > 16 { throw posixError(Int32(ENAMETOOLONG)) }
        if ![0, 1024, 2048, 4096, 65536].contains(blockSize) { throw posixError(Int32(EINVAL)) }
        if reservedPercent != UInt32.max, reservedPercent > 100 { throw posixError(Int32(EINVAL)) }
        self.label = label.map { Array($0.utf8CString) }
        self.ffiOptions = ext4fs_mkfs_options(label: nil, revision: revision, block_size: blockSize, inode_size: inodeSize, inode_ratio: inodeRatio, reserved_percent: reservedPercent)
    }

    func withLabelPointer<T>(_ body: (UnsafePointer<CChar>?) -> T) -> T {
        guard let label else { return body(nil) }
        return label.withUnsafeBufferPointer { body($0.baseAddress) }
    }

    private static func value(after option: String, from iterator: inout IndexingIterator<[String]>) throws -> String {
        guard let value = iterator.next(), !value.hasPrefix("-") else { throw posixError(Int32(EINVAL)) }
        return value
    }

    private static func uint32(after option: String, from iterator: inout IndexingIterator<[String]>) throws -> UInt32 {
        guard let value = UInt32(try Self.value(after: option, from: &iterator)) else { throw posixError(Int32(EINVAL)) }
        return value
    }
}
