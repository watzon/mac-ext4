import Darwin
import FSKit
import Foundation

final class Ext4Item: FSItem {
    let inode: UInt64

    init(inode: UInt64) {
        self.inode = inode
        super.init()
    }
}

final class Ext4Volume: FSVolume, FSVolume.Operations, FSVolume.ReadWriteOperations, FSVolume.XattrOperations, FSVolume.OpenCloseOperations, FSVolume.RenameOperations {
    /// Guards `pointer`. Operations hold it shared, so they run in
    /// parallel (the engine has its own locking); mount and unmount hold it
    /// exclusively, so the handle is never freed under a running call.
    private let handleLock = ReadWriteLock()
    /// Guards `items` and the publication of items to FSKit (see `reply(_:inode:name:)`).
    private let itemLock = NSLock()
    private var pointer: OpaquePointer?
    private let bridge: DeviceBridge
    private let requestedReadOnly: Bool
    private var readOnly: Bool
    private var items: [UInt64: Ext4Item] = [:]

    init(bridge: DeviceBridge, identifier: FSVolume.Identifier, name: FSFileName, requestedReadOnly: Bool) {
        self.bridge = bridge
        self.requestedReadOnly = requestedReadOnly
        self.readOnly = requestedReadOnly
        super.init(volumeID: identifier, volumeName: name)
    }

    var maximumLinkCount: Int { Int(Int16.max) }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
    var maximumXattrSize: Int { 65_536 }
    /// Set from the engine at activation. It must be a real limit: FSKit
    /// derives FILESIZEBITS from it, and UInt64.max came out as 0 bits,
    /// so Finder refused to copy files of 4 GiB and more.
    private var fileSizeLimit: UInt64 = (UInt64(UInt32.max)) << 12
    var maximumFileSize: UInt64 { fileSizeLimit }
    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let capabilities = FSVolume.SupportedCapabilities()
        capabilities.supportsPersistentObjectIDs = true
        capabilities.supportsSymbolicLinks = true
        capabilities.supportsHardLinks = true
        capabilities.supportsJournal = true
        capabilities.supportsActiveJournal = true
        capabilities.supportsSparseFiles = true
        capabilities.supportsZeroRuns = true
        capabilities.supportsFastStatFS = true
        capabilities.supports2TBFiles = true
        capabilities.supports64BitObjectIDs = true
        capabilities.doesNotSupportImmutableFiles = false
        capabilities.doesNotSupportSettingFilePermissions = false
        capabilities.caseFormat = .sensitive
        return capabilities
    }

    var volumeStatistics: FSStatFSResult {
        let result = FSStatFSResult(fileSystemTypeName: "ext4")
        guard let stat = try? statfs() else { return result }
        result.blockSize = Int(stat.block_size)
        result.ioSize = Int(stat.block_size)
        result.totalBlocks = stat.total_blocks
        result.freeBlocks = stat.free_blocks
        result.availableBlocks = stat.avail_blocks
        result.usedBlocks = stat.total_blocks - stat.free_blocks
        result.totalFiles = stat.total_inodes
        result.freeFiles = stat.free_inodes
        // Finder names the format by matching this against FSSubType in the
        // Info.plist personalities; without it Finder shows "Unknown (ext4)".
        switch stringFromCBytes(stat.flavor) {
        case "ext2": result.fileSystemSubType = 2
        case "ext3": result.fileSystemSubType = 3
        default: result.fileSystemSubType = 4
        }
        return result
    }

    @available(macOS 26.4, *)
    var requestedMountOptions: FSVolume.MountOptions {
        readOnly ? .readOnly : []
    }

    func activate(options: FSTaskOptions, replyHandler reply: @escaping (FSItem?, (any Error)?) -> Void) {
        handleLock.lockExclusive()
        defer { handleLock.unlock() }
        if pointer != nil {
            publish(UInt64(EXT4FS_ROOT_INO)) { reply($0, nil) }
            return
        }
        var device = bridge.makeDevice()
        var mounted: OpaquePointer?
        var mountedReadOnly = false
        var message = [CChar](repeating: 0, count: 256)
        let optionReadOnly = Ext4Volume.wantsReadOnly(options.taskOptions)
        ext4Logger.notice("activate options: \(options.taskOptions, privacy: .public)")
        let flags: UInt32 = requestedReadOnly || optionReadOnly ? UInt32(EXT4FS_MOUNT_READ_ONLY) : 0
        let result = message.withUnsafeMutableBufferPointer { messageBuffer in
            withUnsafePointer(to: &device) {
                ext4fs_mount($0, flags, &mounted, &mountedReadOnly, messageBuffer.baseAddress, messageBuffer.count)
            }
        }
        guard result == 0, let mounted else {
            let text = String(cString: message)
            ext4Logger.error("mount failed (\(result, privacy: .public)): \(text, privacy: .public)")
            reply(nil, posixError(result == 0 ? Int32(EIO) : result))
            return
        }
        if mountedReadOnly && !(requestedReadOnly || optionReadOnly) {
            ext4Logger.notice("mounted read-only: \(String(cString: message), privacy: .public)")
        }
        pointer = mounted
        readOnly = requestedReadOnly || optionReadOnly || mountedReadOnly
        var stat = ext4fs_statfs()
        if ext4fs_statfs_get(mounted, &stat) == 0, stat.max_file_size > 0 {
            fileSizeLimit = stat.max_file_size
        }
        publish(UInt64(EXT4FS_ROOT_INO)) { reply($0, nil) }
    }

    /// Read-only requests arrive as `-o` values (from `mount -o rdonly`) or
    /// as standalone flags, depending on the client.
    static func wantsReadOnly(_ options: [String]) -> Bool {
        let words = options.flatMap { $0.split(separator: ",").map { String($0).trimmingCharacters(in: .whitespaces) } }
        return words.contains { ["--rdonly", "-r", "rdonly", "ro", "-o rdonly"].contains($0) }
    }

    func deactivate(options: FSDeactivateOptions, replyHandler reply: @escaping ((any Error)?) -> Void) {
        finishUnmount { error in
            self.itemLock.lock()
            self.items.removeAll()
            self.itemLock.unlock()
            reply(error)
        }
    }

    func mount(options: FSTaskOptions, replyHandler reply: @escaping ((any Error)?) -> Void) {
        reply(nil)
    }

    func unmount(replyHandler reply: @escaping () -> Void) {
        _ = ffiResult { ext4fs_sync($0) }
        reply()
    }

    func finishUnmount(replyHandler reply: @escaping ((any Error)?) -> Void) {
        handleLock.lockExclusive()
        let volume = pointer
        pointer = nil
        handleLock.unlock()
        guard let volume else {
            reply(nil)
            return
        }
        let result = ext4fs_unmount(volume)
        reply(result == 0 ? nil : posixError(result))
    }

    var isActive: Bool {
        handleLock.lockShared()
        defer { handleLock.unlock() }
        return pointer != nil
    }

    func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping ((any Error)?) -> Void) {
        reply(ffiResult { ext4fs_sync($0) })
    }

    func reclaimItem(_ item: FSItem, replyHandler reply: @escaping ((any Error)?) -> Void) {
        guard let ext4Item = item as? Ext4Item else {
            reply(posixError(Int32(EINVAL)))
            return
        }
        var reclaimError: (any Error)?
        itemLock.lock()
        if #available(macOS 27.0, *) {
            let reclaimed = ext4Item.tryReclaim {
                self.items.removeValue(forKey: ext4Item.inode)
                reclaimError = self.ffiResult { ext4fs_reclaim($0, ext4Item.inode) }
            }
            itemLock.unlock()
            reply(reclaimed ? reclaimError : nil)
            return
        }
        items.removeValue(forKey: ext4Item.inode)
        reclaimError = ffiResult { ext4fs_reclaim($0, ext4Item.inode) }
        itemLock.unlock()
        reply(reclaimError)
    }

    func lookupItem(named name: FSFileName, inDirectory directory: FSItem, replyHandler reply: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void) {
        guard let directory = directory as? Ext4Item else {
            reply(nil, nil, posixError(Int32(EINVAL)))
            return
        }
        var attributes = ext4fs_attr()
        let result = name.data.withUnsafeBytes { bytes in
            withVolume { ext4fs_lookup($0, directory.inode, bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count, &attributes) }
        }
        guard result == 0 else {
            reply(nil, nil, posixError(result))
            return
        }
        publish(attributes.ino) { reply($0, name, nil) }
    }

    func createItem(named name: FSFileName, type: FSItem.ItemType, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, replyHandler reply: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void) {
        guard let directory = directory as? Ext4Item else {
            reply(nil, nil, posixError(Int32(EINVAL)))
            return
        }
        let typeBits: UInt32
        switch type {
        case .file: typeBits = 0o100000
        case .directory: typeBits = 0o040000
        default:
            reply(nil, nil, posixError(Int32(EOPNOTSUPP)))
            return
        }
        let mode = typeBits | (newAttributes.isValid(.mode) ? newAttributes.mode & 0o7777 : 0o644)
        let uid = newAttributes.isValid(.uid) ? newAttributes.uid : 0
        let gid = newAttributes.isValid(.gid) ? newAttributes.gid : 0
        var output = ext4fs_attr()
        let result = name.data.withUnsafeBytes { bytes in
            withVolume { ext4fs_create($0, directory.inode, bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count, mode, uid, gid, 0, &output) }
        }
        guard result == 0 else {
            reply(nil, nil, posixError(result))
            return
        }
        publish(output.ino) { reply($0, name, nil) }
    }

    func createSymbolicLink(named name: FSFileName, inDirectory directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents: FSFileName, replyHandler reply: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void) {
        guard let directory = directory as? Ext4Item else {
            reply(nil, nil, posixError(Int32(EINVAL)))
            return
        }
        let uid = newAttributes.isValid(.uid) ? newAttributes.uid : 0
        let gid = newAttributes.isValid(.gid) ? newAttributes.gid : 0
        var output = ext4fs_attr()
        let result = name.data.withUnsafeBytes { nameBytes in
            linkContents.data.withUnsafeBytes { targetBytes in
                withVolume {
                    ext4fs_symlink($0, directory.inode, nameBytes.bindMemory(to: UInt8.self).baseAddress, nameBytes.count, targetBytes.bindMemory(to: UInt8.self).baseAddress, targetBytes.count, uid, gid, &output)
                }
            }
        }
        guard result == 0 else {
            reply(nil, nil, posixError(result))
            return
        }
        publish(output.ino) { reply($0, name, nil) }
    }

    func createLink(to item: FSItem, named name: FSFileName, inDirectory directory: FSItem, replyHandler reply: @escaping (FSFileName?, (any Error)?) -> Void) {
        guard let item = item as? Ext4Item, let directory = directory as? Ext4Item else {
            reply(nil, posixError(Int32(EINVAL)))
            return
        }
        var output = ext4fs_attr()
        let result = name.data.withUnsafeBytes { bytes in
            withVolume { ext4fs_link($0, item.inode, directory.inode, bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count, &output) }
        }
        reply(result == 0 ? name : nil, result == 0 ? nil : posixError(result))
    }

    func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, replyHandler reply: @escaping (FSFileName?, (any Error)?) -> Void) {
        guard let sourceDirectory = sourceDirectory as? Ext4Item, let destinationDirectory = destinationDirectory as? Ext4Item else {
            reply(nil, posixError(Int32(EINVAL)))
            return
        }
        let result = sourceName.data.withUnsafeBytes { sourceBytes in
            destinationName.data.withUnsafeBytes { destinationBytes in
                withVolume {
                    ext4fs_rename($0, sourceDirectory.inode, sourceBytes.bindMemory(to: UInt8.self).baseAddress, sourceBytes.count, destinationDirectory.inode, destinationBytes.bindMemory(to: UInt8.self).baseAddress, destinationBytes.count, 0)
                }
            }
        }
        reply(result == 0 ? destinationName : nil, result == 0 ? nil : posixError(result))
    }

    func removeItem(_ item: FSItem, named name: FSFileName, fromDirectory directory: FSItem, replyHandler reply: @escaping ((any Error)?) -> Void) {
        guard let directory = directory as? Ext4Item else {
            reply(posixError(Int32(EINVAL)))
            return
        }
        let result = name.data.withUnsafeBytes { bytes in
            withVolume { ext4fs_remove($0, directory.inode, bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count) }
        }
        reply(result == 0 ? nil : posixError(result))
    }

    func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, replyHandler reply: @escaping (FSItem.Attributes?, (any Error)?) -> Void) {
        guard let item = item as? Ext4Item else {
            reply(nil, posixError(Int32(EINVAL)))
            return
        }
        var attributes = ext4fs_attr()
        let result = withVolume { ext4fs_getattr($0, item.inode, &attributes) }
        reply(result == 0 ? makeAttributes(attributes, wanted: desiredAttributes) : nil, result == 0 ? nil : posixError(result))
    }

    func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, replyHandler reply: @escaping (FSItem.Attributes?, (any Error)?) -> Void) {
        guard let item = item as? Ext4Item else {
            reply(nil, posixError(Int32(EINVAL)))
            return
        }
        var change = ext4fs_setattr()
        var consumed: FSItem.Attribute = []
        if newAttributes.isValid(.mode) { change.valid |= UInt32(EXT4FS_SET_MODE); change.mode = newAttributes.mode; consumed.insert(.mode) }
        if newAttributes.isValid(.uid) { change.valid |= UInt32(EXT4FS_SET_UID); change.uid = newAttributes.uid; consumed.insert(.uid) }
        if newAttributes.isValid(.gid) { change.valid |= UInt32(EXT4FS_SET_GID); change.gid = newAttributes.gid; consumed.insert(.gid) }
        if newAttributes.isValid(.size) {
            // FSKit: ignore size changes on directories and symbolic links, without an error.
            var current = ext4fs_attr()
            let result = withVolume { ext4fs_getattr($0, item.inode, &current) }
            guard result == 0 else { reply(nil, posixError(result)); return }
            if itemType(mode: current.mode) == .file {
                change.valid |= UInt32(EXT4FS_SET_SIZE)
                change.size = newAttributes.size
            }
            consumed.insert(.size)
        }
        if newAttributes.isValid(.flags) { change.valid |= UInt32(EXT4FS_SET_FLAGS); change.flags = newAttributes.flags; consumed.insert(.flags) }
        if newAttributes.isValid(.accessTime) { change.valid |= UInt32(EXT4FS_SET_ATIME); change.atime = ffiTime(newAttributes.accessTime); consumed.insert(.accessTime) }
        if newAttributes.isValid(.modifyTime) { change.valid |= UInt32(EXT4FS_SET_MTIME); change.mtime = ffiTime(newAttributes.modifyTime); consumed.insert(.modifyTime) }
        if newAttributes.isValid(.changeTime) { change.valid |= UInt32(EXT4FS_SET_CTIME); change.ctime = ffiTime(newAttributes.changeTime); consumed.insert(.changeTime) }
        if newAttributes.isValid(.birthTime) { change.valid |= UInt32(EXT4FS_SET_BTIME); change.btime = ffiTime(newAttributes.birthTime); consumed.insert(.birthTime) }
        var output = ext4fs_attr()
        let result = withVolume { ext4fs_setattr_apply($0, item.inode, &change, &output) }
        if result == 0 { newAttributes.consumedAttributes = consumed }
        reply(result == 0 ? makeAttributes(output, wanted: nil) : nil, result == 0 ? nil : posixError(result))
    }

    func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, replyHandler reply: @escaping (FSDirectoryVerifier, (any Error)?) -> Void) {
        guard let directory = directory as? Ext4Item else {
            reply(FSDirectoryVerifier(rawValue: 0), posixError(Int32(EINVAL)))
            return
        }
        let context = DirectoryEnumerationContext(volume: self, attributes: attributes, packer: packer)
        var currentVerifier: UInt64 = 0
        let result = withVolume { volume in
            ext4fs_readdir(volume, directory.inode, cookie.rawValue, attributes != nil, Ext4Volume.packDirectoryEntry, Unmanaged.passUnretained(context).toOpaque(), &currentVerifier)
        }
        let error = result == 0 ? nil : posixError(result)
        reply(FSDirectoryVerifier(rawValue: currentVerifier), error)
    }

    func readSymbolicLink(_ item: FSItem, replyHandler reply: @escaping (FSFileName?, (any Error)?) -> Void) {
        guard let item = item as? Ext4Item else {
            reply(nil, posixError(Int32(EINVAL)))
            return
        }
        var length = 0
        let first = withVolume { ext4fs_readlink($0, item.inode, nil, 0, &length) }
        guard first == 0 || first == Int32(ERANGE) else {
            reply(nil, posixError(first))
            return
        }
        var bytes = [UInt8](repeating: 0, count: length)
        let result = bytes.withUnsafeMutableBufferPointer { buffer in
            withVolume { ext4fs_readlink($0, item.inode, buffer.baseAddress, buffer.count, &length) }
        }
        reply(result == 0 ? FSFileName(data: Data(bytes.prefix(length))) : nil, result == 0 ? nil : posixError(result))
    }

    func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer, replyHandler reply: @escaping (Int, (any Error)?) -> Void) {
        guard let item = item as? Ext4Item, offset >= 0, length >= 0 else {
            reply(0, posixError(Int32(EINVAL)))
            return
        }
        var readCount = 0
        let result = buffer.withUnsafeMutableBytes { raw in
            withVolume { ext4fs_read($0, item.inode, UInt64(offset), raw.baseAddress, min(length, raw.count), &readCount) }
        }
        reply(readCount, result == 0 ? nil : posixError(result))
    }

    func write(contents: Data, to item: FSItem, at offset: off_t, replyHandler reply: @escaping (Int, (any Error)?) -> Void) {
        guard let item = item as? Ext4Item, offset >= 0 else {
            reply(0, posixError(Int32(EINVAL)))
            return
        }
        var written = 0
        let result = contents.withUnsafeBytes { bytes in
            withVolume { ext4fs_write($0, item.inode, UInt64(offset), bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count, &written) }
        }
        reply(written, result == 0 ? nil : posixError(result))
    }

    func openItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping ((any Error)?) -> Void) {
        if readOnly && modes.contains(.write) {
            reply(posixError(Int32(EROFS)))
        } else {
            reply(nil)
        }
    }

    /// Closing is not a durability request (as on Linux): the engine commits
    /// on its own schedule, on synchronize, and at unmount.
    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler reply: @escaping ((any Error)?) -> Void) {
        reply(item is Ext4Item ? nil : posixError(Int32(EINVAL)))
    }

    func getXattr(named name: FSFileName, of item: FSItem, replyHandler reply: @escaping (Data?, (any Error)?) -> Void) {
        guard let item = item as? Ext4Item else { reply(nil, posixError(Int32(EINVAL))); return }
        var length = 0
        let first = name.data.withUnsafeBytes { nameBytes in
            withVolume { ext4fs_getxattr($0, item.inode, nameBytes.bindMemory(to: UInt8.self).baseAddress, nameBytes.count, nil, 0, &length) }
        }
        guard first == 0 || first == Int32(ERANGE) else { reply(nil, posixError(first)); return }
        var bytes = [UInt8](repeating: 0, count: length)
        let result = name.data.withUnsafeBytes { nameBytes in
            bytes.withUnsafeMutableBufferPointer { valueBytes in
                withVolume { ext4fs_getxattr($0, item.inode, nameBytes.bindMemory(to: UInt8.self).baseAddress, nameBytes.count, valueBytes.baseAddress, valueBytes.count, &length) }
            }
        }
        reply(result == 0 ? Data(bytes.prefix(length)) : nil, result == 0 ? nil : posixError(result))
    }

    func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, replyHandler reply: @escaping ((any Error)?) -> Void) {
        guard let item = item as? Ext4Item else { reply(posixError(Int32(EINVAL))); return }
        let result: Int32
        if policy == .delete {
            result = name.data.withUnsafeBytes { nameBytes in
                withVolume { ext4fs_removexattr($0, item.inode, nameBytes.bindMemory(to: UInt8.self).baseAddress, nameBytes.count) }
            }
        } else {
            guard let value else { reply(posixError(Int32(EINVAL))); return }
            let flags: UInt32 = policy == .mustCreate ? UInt32(EXT4FS_XATTR_CREATE) : (policy == .mustReplace ? UInt32(EXT4FS_XATTR_REPLACE) : 0)
            result = name.data.withUnsafeBytes { nameBytes in
                value.withUnsafeBytes { valueBytes in
                    withVolume { ext4fs_setxattr($0, item.inode, nameBytes.bindMemory(to: UInt8.self).baseAddress, nameBytes.count, valueBytes.bindMemory(to: UInt8.self).baseAddress, valueBytes.count, flags) }
                }
            }
        }
        reply(result == 0 ? nil : posixError(result))
    }

    func listXattrs(of item: FSItem, replyHandler reply: @escaping ([FSFileName]?, (any Error)?) -> Void) {
        guard let item = item as? Ext4Item else { reply(nil, posixError(Int32(EINVAL))); return }
        var length = 0
        let first = withVolume { ext4fs_listxattr($0, item.inode, nil, 0, &length) }
        guard first == 0 || first == Int32(ERANGE) else { reply(nil, posixError(first)); return }
        var bytes = [UInt8](repeating: 0, count: length)
        let result = bytes.withUnsafeMutableBufferPointer { buffer in
            withVolume { ext4fs_listxattr($0, item.inode, buffer.baseAddress, buffer.count, &length) }
        }
        guard result == 0 else { reply(nil, posixError(result)); return }
        let names = bytes.prefix(length).split(separator: 0, omittingEmptySubsequences: true).map { FSFileName(data: Data($0)) }
        reply(names, nil)
    }

    func setVolumeName(_ name: FSFileName, replyHandler reply: @escaping (FSFileName?, (any Error)?) -> Void) {
        guard name.data.count <= 16 else { reply(nil, posixError(Int32(ENAMETOOLONG))); return }
        let result = name.data.withUnsafeBytes { bytes in
            withVolume { ext4fs_set_label($0, bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count) }
        }
        if result == 0 { self.name = name }
        reply(result == 0 ? name : nil, result == 0 ? nil : posixError(result))
    }

    /// Find or make the item for `inode` and hand it to FSKit through `reply`.
    /// The reply runs under `itemLock`, the same lock `reclaimItem` holds
    /// around `tryReclaim`: FSKit counts an item as returned only when the
    /// reply runs, so a reclaim must not slip in between (FSItem.h).
    private func publish(_ inode: UInt64, _ reply: (Ext4Item) -> Void) {
        itemLock.lock()
        defer { itemLock.unlock() }
        if let item = items[inode] {
            reply(item)
            return
        }
        let item = Ext4Item(inode: inode)
        items[inode] = item
        reply(item)
    }

    fileprivate func withVolume(_ body: (OpaquePointer) -> Int32) -> Int32 {
        handleLock.lockShared()
        defer { handleLock.unlock() }
        guard let pointer else { return Int32(ENODEV) }
        return body(pointer)
    }

    private func ffiResult(_ body: (OpaquePointer) -> Int32) -> (any Error)? {
        let result = withVolume(body)
        return result == 0 ? nil : posixError(result)
    }

    private func statfs() throws -> ext4fs_statfs {
        var stat = ext4fs_statfs()
        let result = withVolume { ext4fs_statfs_get($0, &stat) }
        guard result == 0 else { throw posixError(result) }
        return stat
    }

    fileprivate func makeAttributes(_ attr: ext4fs_attr, wanted: FSItem.GetAttributesRequest?) -> FSItem.Attributes {
        let result = FSItem.Attributes()
        func wants(_ attribute: FSItem.Attribute) -> Bool { wanted == nil || wanted!.isAttributeWanted(attribute) }
        if wants(.type) { result.type = itemType(mode: attr.mode) }
        if wants(.mode) { result.mode = attr.mode }
        if wants(.linkCount) { result.linkCount = attr.nlink }
        if wants(.uid) { result.uid = attr.uid }
        if wants(.gid) { result.gid = attr.gid }
        if wants(.flags) { result.flags = attr.flags }
        if wants(.size) { result.size = attr.size }
        if wants(.allocSize) { result.allocSize = attr.alloc_size }
        if wants(.fileID) { result.fileID = FSItem.Identifier(attr.ino) }
        // The engine knows the parent of directories only (an inode does not
        // record the directory of a file, which can have several links).
        if wants(.parentID), attr.parent_ino != 0 { result.parentID = FSItem.Identifier(attr.parent_ino) }
        if wants(.accessTime) { result.accessTime = posixTime(attr.atime) }
        if wants(.modifyTime) { result.modifyTime = posixTime(attr.mtime) }
        if wants(.changeTime) { result.changeTime = posixTime(attr.ctime) }
        if wants(.birthTime), attr.has_btime { result.birthTime = posixTime(attr.btime) }
        return result
    }

    private static let packDirectoryEntry: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, Int, UInt64, UInt8, UInt64, UnsafePointer<ext4fs_attr>?) -> Bool = { context, name, nameLength, inode, fileType, nextCookie, attr in
        guard let context, let name, nameLength >= 0 else { return false }
        let state = Unmanaged<DirectoryEnumerationContext>.fromOpaque(context).takeUnretainedValue()
        return state.pack(name: name, length: nameLength, inode: inode, fileType: fileType, nextCookie: nextCookie, attr: attr)
    }
}

private final class DirectoryEnumerationContext {
    let volume: Ext4Volume
    let attributes: FSItem.GetAttributesRequest?
    let packer: FSDirectoryEntryPacker

    init(volume: Ext4Volume, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker) {
        self.volume = volume
        self.attributes = attributes
        self.packer = packer
    }

    /// Runs inside the engine's enumeration, which holds the volume locked:
    /// it must not call back into the volume.
    func pack(name: UnsafePointer<UInt8>, length: Int, inode: UInt64, fileType: UInt8, nextCookie: UInt64, attr: UnsafePointer<ext4fs_attr>?) -> Bool {
        let entryAttributes: FSItem.Attributes?
        if let attributes {
            // FSKit wants no "." and ".." when it asks for attributes; they
            // are the entries that come without them.
            guard let attr else { return true }
            entryAttributes = volume.makeAttributes(attr.pointee, wanted: attributes)
        } else {
            entryAttributes = nil
        }
        let data = Data(bytes: name, count: length)
        return packer.packEntry(name: FSFileName(data: data), itemType: itemType(directoryType: fileType), itemID: FSItem.Identifier(inode), nextCookie: FSDirectoryCookie(rawValue: nextCookie), attributes: entryAttributes)
    }
}

private func itemType(mode: UInt32) -> FSItem.ItemType {
    switch mode & 0o170000 {
    case 0o100000: return .file
    case 0o040000: return .directory
    case 0o120000: return .symlink
    case 0o010000: return .fifo
    case 0o020000: return .charDevice
    case 0o060000: return .blockDevice
    case 0o140000: return .socket
    default: return .unknown
    }
}

private func itemType(directoryType: UInt8) -> FSItem.ItemType {
    switch directoryType {
    case UInt8(EXT4FS_DT_REG): return .file
    case UInt8(EXT4FS_DT_DIR): return .directory
    case UInt8(EXT4FS_DT_LNK): return .symlink
    case UInt8(EXT4FS_DT_FIFO): return .fifo
    case UInt8(EXT4FS_DT_CHR): return .charDevice
    case UInt8(EXT4FS_DT_BLK): return .blockDevice
    case UInt8(EXT4FS_DT_SOCK): return .socket
    default: return .unknown
    }
}

private func posixTime(_ time: ext4fs_timespec) -> timespec {
    timespec(tv_sec: Int(time.sec), tv_nsec: Int(time.nsec))
}

private func ffiTime(_ time: timespec) -> ext4fs_timespec {
    ext4fs_timespec(sec: Int64(time.tv_sec), nsec: UInt32(clamping: time.tv_nsec))
}

/// A pthread read-write lock (NSLock has no shared mode).
private final class ReadWriteLock {
    private let lock = UnsafeMutablePointer<pthread_rwlock_t>.allocate(capacity: 1)

    init() {
        lock.initialize(to: pthread_rwlock_t())
        pthread_rwlock_init(lock, nil)
    }

    deinit {
        pthread_rwlock_destroy(lock)
        lock.deinitialize(count: 1)
        lock.deallocate()
    }

    func lockShared() { pthread_rwlock_rdlock(lock) }
    func lockExclusive() { pthread_rwlock_wrlock(lock) }
    func unlock() { pthread_rwlock_unlock(lock) }
}
