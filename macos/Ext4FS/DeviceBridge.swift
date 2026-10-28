import Darwin
import FSKit
import Foundation
import OSLog

let ext4Logger = Logger(subsystem: Bundle.main.bundleIdentifier ?? "dev.watzon.macext4.fsmodule", category: "filesystem")

func posixError(_ code: Int32) -> NSError {
    fs_errorForPOSIXError(code > 0 ? code : Int32(EIO)) as NSError
}

func errnoFromError(_ error: Error) -> Int32 {
    let nsError = error as NSError
    if nsError.domain == NSPOSIXErrorDomain, nsError.code > 0, nsError.code <= Int(Int32.max) {
        return Int32(nsError.code)
    }
    return Int32(EIO)
}

func stringFromCBytes<T>(_ value: T) -> String where T: Any {
    withUnsafeBytes(of: value) { raw in
        let bytes = raw.prefix { $0 != 0 }
        return String(bytes: bytes, encoding: .utf8) ?? ""
    }
}

func uuidFromCBytes(_ bytes: (UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8, UInt8)) -> UUID {
    withUnsafeBytes(of: bytes) { raw in
        UUID(uuid: (
            raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
            raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15]
        ))
    }
}

/// Owns the direct-I/O callbacks used by the Rust engine. All engine I/O uses
/// `FSBlockDeviceResource` direct operations, so it never mixes direct and
/// metadata cache APIs on the same device ranges. FSKit exposes no direct-I/O
/// flush API; flush is intentionally a no-op and journal durability relies on
/// FSKit's completed synchronous writes and ext4fs_sync ordering.
final class DeviceBridge {
    let resource: FSBlockDeviceResource
    private let deviceSize: UInt64
    private let alignment: UInt32

    init(resource: FSBlockDeviceResource) {
        self.resource = resource
        let product = resource.blockCount.multipliedReportingOverflow(by: resource.blockSize)
        self.deviceSize = product.overflow ? UInt64.max : product.partialValue
        let physical = max(resource.physicalBlockSize, 1)
        self.alignment = physical > UInt64(UInt32.max) ? UInt32.max : UInt32(physical)
    }

    func makeDevice() -> ext4fs_device {
        var device = ext4fs_device()
        device.ctx = Unmanaged.passUnretained(self).toOpaque()
        device.read = DeviceBridge.read
        device.write = DeviceBridge.write
        device.flush = DeviceBridge.flush
        device.size = deviceSize
        device.alignment = alignment
        device.read_only = !resource.isWritable
        return device
    }

    private static func bridge(_ context: UnsafeMutableRawPointer?) -> DeviceBridge? {
        guard let context else { return nil }
        return Unmanaged<DeviceBridge>.fromOpaque(context).takeUnretainedValue()
    }

    private static let read: @convention(c) (UnsafeMutableRawPointer?, UInt64, UnsafeMutableRawPointer?, Int) -> Int32 = { context, offset, buffer, length in
        guard let bridge = bridge(context), length >= 0 else { return Int32(EINVAL) }
        guard length == 0 || buffer != nil, offset <= UInt64(Int64.max) else { return Int32(EINVAL) }
        do {
            let rawBuffer = UnsafeMutableRawBufferPointer(start: buffer, count: length)
            let transferred = try bridge.resource.read(into: rawBuffer, startingAt: off_t(offset), length: length)
            return transferred == length ? 0 : Int32(EIO)
        } catch {
            return errnoFromError(error)
        }
    }

    private static let write: @convention(c) (UnsafeMutableRawPointer?, UInt64, UnsafeRawPointer?, Int) -> Int32 = { context, offset, buffer, length in
        guard let bridge = bridge(context), length >= 0 else { return Int32(EINVAL) }
        guard length == 0 || buffer != nil, offset <= UInt64(Int64.max) else { return Int32(EINVAL) }
        do {
            let rawBuffer = UnsafeRawBufferPointer(start: buffer, count: length)
            let transferred = try bridge.resource.write(from: rawBuffer, startingAt: off_t(offset), length: length)
            return transferred == length ? 0 : Int32(EIO)
        } catch {
            return errnoFromError(error)
        }
    }

    private static let flush: @convention(c) (UnsafeMutableRawPointer?) -> Int32 = { _ in
        0
    }
}
