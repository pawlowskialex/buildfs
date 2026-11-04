import CryptoKit
import ExtensionFoundation
import Foundation
import FSKit
import os

// FSKit front end for buildfs: a thin mapping from FSKit's volume API onto
// the Rust core (crates/buildfs-ffi, header buildfs_ffi.h).
//
// Threading: FSKit delivers every call on one serial queue, and that queue is
// the per-volume throughput limit (see docs/HANDOFF-fskit.md). Cheap
// metadata calls run inline on it; calls that can block for long
// (decompression, rehydration, flush barriers) go to `slowQueue`.

@main
struct BuildFSExtension: UnaryFileSystemExtension {
    let fileSystem = BuildFSFileSystem()
}

let log = Logger(subsystem: "com.pawlowskialex.buildfs.fsmodule", category: "fs")
let coreLog = Logger(subsystem: "com.pawlowskialex.buildfs.fsmodule", category: "core")

/// Forwards the Rust core's tracing output to the unified log.
private let coreLogSink: BfsLogFn = { _, level, msg, len in
    guard let msg else { return }
    let s = String(decoding: UnsafeBufferPointer(start: msg, count: len), as: UTF8.self)
    switch level {
    case BFS_LOG_ERROR: coreLog.error("\(s, privacy: .public)")
    case BFS_LOG_WARN: coreLog.warning("\(s, privacy: .public)")
    case BFS_LOG_INFO: coreLog.notice("\(s, privacy: .public)")
    default: coreLog.debug("\(s, privacy: .public)")
    }
}

/// Per-handler call counts and time, logged every few seconds while busy.
final class Stats: @unchecked Sendable {
    static let shared = Stats()
    private let lock = OSAllocatedUnfairLock()
    private var calls: [String: (n: Int, ns: UInt64)] = [:]
    private var lastDump = DispatchTime.now().uptimeNanoseconds

    func record(_ name: String, since start: UInt64) {
        let now = DispatchTime.now().uptimeNanoseconds
        var dump: String?
        lock.withLockUnchecked {
            var e = calls[name] ?? (0, 0)
            e.n += 1
            e.ns += now - start
            calls[name] = e
            if now - lastDump > 3_000_000_000 {
                lastDump = now
                dump = calls.sorted { $0.value.n > $1.value.n }
                    .map { "\($0.key)=\($0.value.n)/\($0.value.ns / UInt64(max($0.value.n, 1)) / 1000)us" }
                    .joined(separator: " ")
                calls.removeAll()
            }
        }
        if let dump { log.notice("stats \(dump, privacy: .public)") }
    }
}

@inline(__always) func t0() -> UInt64 { DispatchTime.now().uptimeNanoseconds }

func fsError(_ rc: Int32) -> Error { POSIXError(POSIXErrorCode(rawValue: rc) ?? .EIO) }

func posix(_ code: POSIXErrorCode) -> Error { POSIXError(code) }

/// Store names are the URL host: `buildfs://default` -> store `default`.
func validStoreName(_ s: String) -> Bool {
    !s.isEmpty && s.count <= 64 && !s.hasPrefix(".")
        && s.unicodeScalars.allSatisfy { CharacterSet.alphanumerics.contains($0) || "-_.".unicodeScalars.contains($0) }
}

/// A stable volume UUID per store name.
func volumeUUID(_ name: String) -> UUID {
    var b = Array(Insecure.MD5.hash(data: Data("buildfs:\(name)".utf8)))
    b[6] = (b[6] & 0x0F) | 0x30 // RFC 4122 version 3 (name-based, MD5)
    b[8] = (b[8] & 0x3F) | 0x80
    return UUID(uuid: (b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]))
}

final class BuildFSFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations, @unchecked Sendable {
    private let lock = OSAllocatedUnfairLock()
    private var volume: BuildFSVolume?

    override init() {
        super.init()
        _ = bfs_set_logger(coreLogSink, nil, "info")
    }

    private func storeName(_ resource: FSResource) -> String? {
        guard let host = (resource as? FSGenericURLResource)?.url.host(), validStoreName(host) else { return nil }
        return host
    }

    func probeResource(resource: FSResource, replyHandler reply: @escaping @Sendable (FSProbeResult?, (any Error)?) -> Void) {
        guard let name = storeName(resource) else { return reply(.notRecognized, nil) }
        reply(.usable(name: name, containerID: FSContainerIdentifier(uuid: volumeUUID(name))), nil)
    }

    func loadResource(resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSVolume?, (any Error)?) -> Void) {
        guard let name = storeName(resource) else {
            log.error("loadResource: unusable resource \(String(describing: resource), privacy: .public)")
            return reply(nil, posix(.EINVAL))
        }
        let v = BuildFSVolume(storeName: name)
        lock.withLockUnchecked { volume = v }
        containerStatus = .ready
        log.notice("loadResource store=\(name, privacy: .public)")
        reply(v, nil)
    }

    func unloadResource(resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        let v = lock.withLockUnchecked { () -> BuildFSVolume? in defer { volume = nil }; return volume }
        v?.close()
        reply(nil)
    }
}

/// One file system object; FSKit identifies items by object identity, so
/// there is exactly one `Node` per inode while the kernel holds it.
final class Node: FSItem, @unchecked Sendable {
    let ino: UInt64
    // Guarded by the volume's lock.
    /// Whether the store was told about an open (`bfs_open_acquired`).
    var isOpen = false
    /// Last directory this item was seen in (reported as parentID for
    /// non-directories, which the store doesn't track).
    var parent: UInt64

    init(ino: UInt64, parent: UInt64) {
        self.ino = ino
        self.parent = parent
        super.init()
    }
}

@inline(__always) func fsID(_ ino: UInt64) -> FSItem.Identifier {
    ino == UInt64(BFS_ROOT_INO) ? .rootDirectory : (FSItem.Identifier(rawValue: ino) ?? .invalid)
}

@inline(__always) func ts(_ t: BfsTime) -> timespec { timespec(tv_sec: Int(t.sec), tv_nsec: Int(t.nsec)) }

@inline(__always) func bfsTime(_ t: timespec) -> BfsTime { BfsTime(sec: Int64(t.tv_sec), nsec: Int64(t.tv_nsec)) }

/// Converts store attributes; `parent` is used for non-directories.
func fsAttrs(_ a: BfsAttr, parent: UInt64) -> FSItem.Attributes {
    let r = FSItem.Attributes()
    r.type = FSItem.ItemType(rawValue: Int(a.kind)) ?? .unknown
    r.mode = a.mode
    r.uid = a.uid
    r.gid = a.gid
    r.linkCount = a.nlink
    r.flags = a.flags
    r.size = a.size
    r.allocSize = a.space_used
    r.fileID = fsID(a.ino)
    if a.ino == UInt64(BFS_ROOT_INO) {
        r.parentID = .parentOfRoot
    } else {
        r.parentID = fsID(a.kind == UInt32(BFS_KIND_DIR) ? a.parent : parent)
    }
    r.accessTime = ts(a.atime)
    r.modifyTime = ts(a.mtime)
    r.changeTime = ts(a.ctime)
    r.birthTime = ts(a.btime)
    r.addedTime = ts(a.btime)
    r.backupTime = timespec()
    r.supportsLimitedXAttrs = false
    return r
}

@inline(__always) func withName<R>(_ n: FSFileName, _ body: (UnsafePointer<UInt8>?, Int) -> R) -> R {
    n.data.withUnsafeBytes { body($0.bindMemory(to: UInt8.self).baseAddress, $0.count) }
}

/// Copies and frees a buffer returned by the core.
func takeBuf(_ b: BfsBuf) -> Data {
    defer { bfs_buf_free(b) }
    guard let p = b.ptr, b.len > 0 else { return Data() }
    return Data(bytes: p, count: b.len)
}

/// Context for `bfs_readdir`'s per-entry callback.
private final class DirPack {
    let packer: FSDirectoryEntryPacker
    let dir: UInt64
    init(packer: FSDirectoryEntryPacker, dir: UInt64) { self.packer = packer; self.dir = dir }
}

private let packEntry: BfsDirEmit = { ctx, name, len, ino, kind, cookie, attr in
    guard let ctx, let name else { return false }
    let box = Unmanaged<DirPack>.fromOpaque(ctx).takeUnretainedValue()
    let a = attr.map { fsAttrs($0.pointee, parent: box.dir) }
    return box.packer.packEntry(
        name: FSFileName(data: Data(bytes: name, count: len)),
        itemType: FSItem.ItemType(rawValue: Int(kind)) ?? .unknown,
        itemID: fsID(ino),
        nextCookie: FSDirectoryCookie(cookie),
        attributes: a)
}

final class BuildFSVolume: FSVolume, FSVolume.Handler, FSVolume.ReadWriteHandler, FSVolume.DataCacheHandler, FSVolume.XattrHandler, @unchecked Sendable {
    let storeName: String
    /// The open store; set in activateVolume, cleared by close().
    private var v: OpaquePointer?
    private let lock = OSAllocatedUnfairLock()
    private var items: [UInt64: Node] = [:]
    private var root: Node?
    private var groupCache: [UInt32: [UInt32]] = [:]
    /// `-o trace`: log every lookup and xattr read with its result.
    private var trace = false
    /// `-o discard`: delete the store when the volume closes (`buildfs detach`).
    private var discardOnClose = false
    private let slowQueue = DispatchQueue(label: "buildfs.slow", qos: .userInitiated, attributes: .concurrent)

    init(storeName: String) {
        self.storeName = storeName
        super.init(volumeID: FSVolume.Identifier(uuid: volumeUUID(storeName)), volumeName: FSFileName(string: storeName))
    }

    private var storeDir: String {
        (NSHomeDirectory() as NSString).appendingPathComponent("stores/\(storeName)")
    }

    /// Closes the store (idempotent), logging what it holds.
    func close() {
        let h = lock.withLockUnchecked { () -> OpaquePointer? in defer { v = nil }; return v }
        guard let h else { return }
        var st = BfsCompressStats()
        _ = bfs_compress_stats(h, &st)
        bfs_close(h)
        log.notice("""
            closed store \(self.storeName, privacy: .public): \(Self.diskUsage(self.storeDir)) bytes on disk; \
            this session compacted \(st.compacted) files (\(st.logical_bytes) -> \(st.stored_bytes) bytes, \
            \(st.dedup_chunks) chunks deduplicated)
            """)
        if discardOnClose {
            try? FileManager.default.removeItem(atPath: storeDir)
            log.notice("discarded store \(self.storeName, privacy: .public)")
        }
    }

    /// Allocated bytes under `dir` (what `du` reports). The store lives in
    /// the sandbox container, which other apps can't read.
    static func diskUsage(_ dir: String) -> UInt64 {
        let keys: [URLResourceKey] = [.totalFileAllocatedSizeKey, .isRegularFileKey]
        guard let e = FileManager.default.enumerator(at: URL(fileURLWithPath: dir), includingPropertiesForKeys: keys) else { return 0 }
        var total: UInt64 = 0
        for case let url as URL in e {
            if let r = try? url.resourceValues(forKeys: Set(keys)), r.isRegularFile == true {
                total += UInt64(r.totalFileAllocatedSize ?? 0)
            }
        }
        return total
    }

    // MARK: plumbing

    @inline(__always) private func run(_ name: String, _ body: () -> Void) {
        let t = t0(); body(); Stats.shared.record(name, since: t)
    }

    private func slow(_ name: String, _ body: @escaping () -> Void) {
        slowQueue.async { let t = t0(); body(); Stats.shared.record(name, since: t) }
    }

    /// The item for `ino`, creating it if the kernel doesn't hold one.
    /// Callers hold `lock`.
    private func node(_ ino: UInt64, parent: UInt64) -> Node {
        if let n = items[ino] {
            n.parent = parent
            return n
        }
        let n = Node(ino: ino, parent: parent)
        items[ino] = n
        return n
    }

    private func item(_ a: BfsAttr, parent: UInt64) -> Node {
        lock.withLockUnchecked { node(a.ino, parent: parent) }
    }

    private func attrs(_ a: BfsAttr, of n: Node) -> FSItem.Attributes {
        fsAttrs(a, parent: lock.withLockUnchecked { n.parent })
    }

    /// Supplementary groups for `uid`. FSContext carries only the effective
    /// uid and gid; the groups matter for chgrp by the owner.
    private func groups(uid: UInt32, gid: UInt32) -> [UInt32] {
        if let g = lock.withLockUnchecked({ groupCache[uid] }) { return g }
        var result = [gid]
        if let pw = getpwuid(uid) {
            var n: Int32 = 64
            var buf = [Int32](repeating: 0, count: 64)
            if getgrouplist(pw.pointee.pw_name, Int32(bitPattern: gid), &buf, &n) >= 0 {
                result = buf.prefix(Int(n)).map { UInt32(bitPattern: $0) }
            }
        }
        lock.withLockUnchecked { groupCache[uid] = result }
        return result
    }

    private func withCaller<R>(_ ctx: FSContext, _ body: (UnsafePointer<BfsCaller>) -> R) -> R {
        let uid = UInt32(truncatingIfNeeded: ctx.effectiveUserID)
        let gid = UInt32(truncatingIfNeeded: ctx.effectiveGroupID)
        let g = groups(uid: uid, gid: gid)
        return g.withUnsafeBufferPointer { gp in
            var c = BfsCaller(uid: uid, gid: gid, groups: gp.baseAddress, ngroups: gp.count)
            return withUnsafePointer(to: &c) { body($0) }
        }
    }

    /// Translates an FSKit set-attributes request; also returns what counts
    /// as consumed on success.
    private func setattr(_ r: FSItem.SetAttributesRequest) -> (BfsSetattr, FSItem.Attribute) {
        var s = BfsSetattr()
        var used: FSItem.Attribute = []
        func take(_ a: FSItem.Attribute, _ bit: Int32) -> Bool {
            guard r.isValid(a) else { return false }
            s.valid |= UInt32(bit)
            used.insert(a)
            return true
        }
        if take(.size, BFS_SET_SIZE) { s.size = r.size }
        if take(.mode, BFS_SET_MODE) { s.mode = r.mode }
        if take(.uid, BFS_SET_UID) { s.uid = r.uid }
        if take(.gid, BFS_SET_GID) { s.gid = r.gid }
        if take(.flags, BFS_SET_FLAGS) { s.flags = r.flags }
        if take(.accessTime, BFS_SET_ATIME) { s.atime = bfsTime(r.accessTime) }
        if take(.modifyTime, BFS_SET_MTIME) { s.mtime = bfsTime(r.modifyTime) }
        if take(.birthTime, BFS_SET_BTIME) { s.btime = bfsTime(r.birthTime) }
        return (s, used)
    }

    // MARK: volume

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let c = FSVolume.SupportedCapabilities()
        c.supportsPersistentObjectIDs = true
        c.supportsSymbolicLinks = true
        c.supportsHardLinks = true
        c.supportsSparseFiles = true
        c.supports64BitObjectIDs = true
        c.supports2TBFiles = true
        c.supportsHiddenFiles = true
        c.supportsFastStatFS = true
        c.caseFormat = .sensitive
        return c
    }

    var volumeStatistics: FSStatFSResult {
        let s = FSStatFSResult(fileSystemTypeName: "buildfs")
        var st = BfsStatfs()
        guard let h = lock.withLockUnchecked({ v }), bfs_statfs(h, &st) == 0, st.block_size > 0 else { return s }
        let bs = st.block_size
        s.blockSize = Int(bs)
        s.ioSize = 1 << 20
        s.totalBlocks = st.total_bytes / bs
        s.freeBlocks = st.free_bytes / bs
        s.availableBlocks = st.avail_bytes / bs
        s.usedBlocks = (st.total_bytes - st.free_bytes) / bs
        s.totalBytes = st.total_bytes
        s.freeBytes = st.free_bytes
        s.availableBytes = st.avail_bytes
        s.usedBytes = st.total_bytes - st.free_bytes
        s.totalFiles = st.total_files
        s.freeFiles = st.free_files
        return s
    }

    // FSKit's headers ask for one of each pair. Without them
    // _PC_FILESIZEBITS reads 0 and _PC_XATTR_SIZE_BITS is ENOTSUP; xattrs
    // and large files work either way. Use the *InBits variants: with
    // maximumFileSize = 2^50 every write fails with EFBIG.
    // _PC_XATTR_SIZE_BITS reads 18 whatever value is returned here.
    /// 64 MiB = 2^(27 - 1); matches BFS_MAX_XATTR_SIZE.
    var maximumXattrSizeInBits: Int { 27 }
    /// 2^50 bytes = 2^(51 - 1); matches BFS_MAX_FILE_SIZE.
    var maximumFileSizeInBits: Int { 51 }
    var maximumLinkCount: Int { 32767 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }

    func activateVolume(options: FSTaskOptions, replyHandler reply: @escaping @Sendable (FSActivateResult?, (any Error)?) -> Void) {
        let dir = storeDir
        // -o options arrive here (not in loadResource) as ["-o", "a,b", ...].
        let mountOpts = options.taskOptions.enumerated()
            .filter { $0.offset > 0 && options.taskOptions[$0.offset - 1] == "-o" }
            .flatMap { $0.element.split(separator: ",").map(String.init) }
        if mountOpts.contains("fresh") {
            // Tests: start from an empty store.
            try? FileManager.default.removeItem(atPath: dir)
            log.notice("fresh: removed store \(dir, privacy: .public)")
        }
        discardOnClose = mountOpts.contains("discard")
        trace = mountOpts.contains("trace")
        // Tuning from `buildfs mount --fskit --level ...`.
        func value(_ key: String) -> Int? {
            mountOpts.first { $0.hasPrefix(key + "=") }.flatMap { Int($0.dropFirst(key.count + 1)) }
        }
        var opts = BfsOptions()
        if let n = value("level") { opts.zstd_level = Int32(n) }
        if let n = value("workers"), n > 0 { opts.compress_workers = UInt32(n) }
        // Seconds without writes before compressing; 0 = never.
        if let n = value("compress_after") { opts.compress_after_ms = n == 0 ? -1 : Int64(n) * 1000 }
        var h: OpaquePointer?
        let rc = dir.withCString { bfs_open($0, &opts, &h) }
        guard rc == 0, let h else {
            log.error("activateVolume: opening \(dir, privacy: .public) failed: \(rc)")
            return reply(nil, fsError(rc))
        }
        let r = lock.withLockUnchecked { () -> Node in
            v = h
            let n = node(UInt64(BFS_ROOT_INO), parent: UInt64(BFS_ROOT_INO))
            root = n
            return n
        }
        log.notice("activated store \(self.storeName, privacy: .public) at \(dir, privacy: .public) options=\(options.taskOptions, privacy: .public)")
        reply(FSActivateResult(rootItem: r), nil)
    }

    func deactivateVolume(options: FSDeactivateOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        close()
        reply(nil)
    }

    func mount(options: FSTaskOptions, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) { reply(nil) }
    func unmount(replyHandler reply: @escaping @Sendable () -> Void) { reply() }

    func synchronize(flags: FSSyncFlags, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        // Waits for a durable metadata commit: keep it off the delivery queue.
        slow("synchronize") { [self] in
            guard let h = v else { return reply(nil) }
            let rc = bfs_sync(h)
            reply(rc == 0 ? nil : fsError(rc))
        }
    }

    func reclaimItem(_ item: FSItem, replyHandler reply: @escaping @Sendable ((any Error)?) -> Void) {
        run("reclaimItem") {
            guard let n = item as? Node else { return reply(nil) }
            var release = false
            // Same lock as lookups: tryReclaim refuses if a lookup just
            // handed this item out again.
            lock.withLockUnchecked {
                _ = n.tryReclaim {
                    if self.items[n.ino] === n { self.items.removeValue(forKey: n.ino) }
                    // Reclaimed without a close: end the open state so an
                    // unlinked inode can be freed.
                    release = n.isOpen
                    n.isOpen = false
                }
            }
            if release, let h = v { bfs_open_released(h, n.ino) }
            reply(nil)
        }
    }

    // MARK: namespace

    func lookupItem(named name: FSFileName, in directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSLookupItemResult?, (any Error)?) -> Void) {
        run("lookupItem") {
            guard let h = v, let dir = directory as? Node else { return reply(nil, posix(.EIO)) }
            var a = BfsAttr()
            let rc = withName(name) { bfs_lookup(h, dir.ino, $0, $1, &a) }
            if trace { log.notice("trace lookup \(name.string ?? "?", privacy: .public) rc=\(rc)") }
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            let n = item(a, parent: dir.ino)
            reply(FSLookupItemResult(foundItem: n, itemName: name, itemAttributes: fsAttrs(a, parent: dir.ino)), nil)
        }
    }

    private func create(_ name: FSFileName, kind: Int32, in directory: FSItem, attributes req: FSItem.SetAttributesRequest,
                        target: FSFileName?, context: FSContext) -> Result<(Node, BfsAttr, BfsAttr, UInt64), Error> {
        guard let h = v, let dir = directory as? Node else { return .failure(posix(.EIO)) }
        var (s, used) = setattr(req)
        // Ownership follows BSD rules: the caller's uid and the parent's gid.
        s.valid &= ~(UInt32(BFS_SET_UID) | UInt32(BFS_SET_GID))
        var (a, d) = (BfsAttr(), BfsAttr())
        let t = target?.data ?? Data()
        let rc = withCaller(context) { who in
            withName(name) { np, nl in
                withUnsafePointer(to: &s) { sp in
                    t.withUnsafeBytes { tp in
                        bfs_create(h, who, dir.ino, np, nl, UInt32(kind), 0, 0, sp,
                                   tp.bindMemory(to: UInt8.self).baseAddress, tp.count, &a, &d)
                    }
                }
            }
        }
        guard rc == 0 else { return .failure(fsError(rc)) }
        // uid/gid count as consumed when they match what the store chose.
        if req.isValid(.uid) && req.uid != a.uid { used.remove(.uid) }
        if req.isValid(.gid) && req.gid != a.gid { used.remove(.gid) }
        req.consumedAttributes = used
        return .success((item(a, parent: dir.ino), a, d, dir.ino))
    }

    func createItem(named name: FSFileName, type: FSItem.ItemType, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateItemResult?, (any Error)?) -> Void) {
        run("createItem") {
            switch create(name, kind: Int32(type.rawValue), in: directory, attributes: newAttributes, target: nil, context: context) {
            case .failure(let e): reply(nil, e)
            case .success(let (n, a, d, dir)):
                reply(FSCreateItemResult(newItem: n, newItemName: name, newItemAttributes: fsAttrs(a, parent: dir),
                                         directoryAttributes: fsAttrs(d, parent: dir), freeSpace: nil), nil)
            }
        }
    }

    func createSymbolicLink(named name: FSFileName, in directory: FSItem, attributes newAttributes: FSItem.SetAttributesRequest, linkContents contents: FSFileName, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateSymlinkResult?, (any Error)?) -> Void) {
        run("createSymbolicLink") {
            switch create(name, kind: BFS_KIND_SYMLINK, in: directory, attributes: newAttributes, target: contents, context: context) {
            case .failure(let e): reply(nil, e)
            case .success(let (n, a, d, dir)):
                reply(FSCreateSymlinkResult(newItem: n, newItemName: name, newItemAttributes: fsAttrs(a, parent: dir),
                                            directoryAttributes: fsAttrs(d, parent: dir), freeSpace: nil), nil)
            }
        }
    }

    func createLink(to item: FSItem, named name: FSFileName, in directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSCreateLinkResult?, (any Error)?) -> Void) {
        run("createLink") {
            guard let h = v, let dir = directory as? Node, let n = item as? Node else { return reply(nil, posix(.EIO)) }
            var (a, d) = (BfsAttr(), BfsAttr())
            let rc = withName(name) { bfs_link(h, n.ino, dir.ino, $0, $1, &a, &d) }
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            lock.withLockUnchecked { n.parent = dir.ino }
            reply(FSCreateLinkResult(linkName: name, linkAttributes: fsAttrs(a, parent: dir.ino),
                                     directoryAttributes: fsAttrs(d, parent: dir.ino), freeSpace: nil), nil)
        }
    }

    func removeItem(_ item: FSItem, named name: FSFileName, from directory: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSRemoveItemResult?, (any Error)?) -> Void) {
        run("removeItem") {
            guard let h = v, let dir = directory as? Node, let n = item as? Node else { return reply(nil, posix(.EIO)) }
            var (a, d) = (BfsAttr(), BfsAttr())
            let rc = withName(name) { bfs_remove(h, dir.ino, $0, $1, &a, &d) }
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            reply(FSRemoveItemResult(itemAttributes: attrs(a, of: n), directoryAttributes: fsAttrs(d, parent: dir.ino), freeSpace: nil), nil)
        }
    }

    func renameItem(_ item: FSItem, inDirectory sourceDirectory: FSItem, named sourceName: FSFileName, to destinationName: FSFileName, inDirectory destinationDirectory: FSItem, overItem: FSItem?, context: FSContext, replyHandler reply: @escaping @Sendable (FSRenameItemResult?, (any Error)?) -> Void) {
        run("renameItem") {
            guard let h = v, let src = sourceDirectory as? Node, let dst = destinationDirectory as? Node, let n = item as? Node else {
                return reply(nil, posix(.EIO))
            }
            var (a, fd, td, o) = (BfsAttr(), BfsAttr(), BfsAttr(), BfsAttr())
            var replaced = false
            let rc = withName(sourceName) { sp, sl in
                withName(destinationName) { dp, dl in
                    bfs_rename(h, src.ino, sp, sl, dst.ino, dp, dl, &a, &fd, &td, &o, &replaced)
                }
            }
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            lock.withLockUnchecked { n.parent = dst.ino }
            let over = replaced ? fsAttrs(o, parent: dst.ino) : nil
            reply(FSRenameItemResult(newName: destinationName, renamedItemAttributes: fsAttrs(a, parent: dst.ino),
                                     sourceDirectoryAttributes: fsAttrs(fd, parent: src.ino),
                                     destinationDirectoryAttributes: fsAttrs(td, parent: dst.ino),
                                     overItemAttributes: over, freeSpace: nil), nil)
        }
    }

    func getAttributes(_ desiredAttributes: FSItem.GetAttributesRequest, of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSGetAttributesResult?, (any Error)?) -> Void) {
        run("getAttributes") {
            guard let h = v, let n = item as? Node else { return reply(nil, posix(.EIO)) }
            var a = BfsAttr()
            let rc = bfs_getattr(h, n.ino, &a)
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            reply(FSGetAttributesResult(attributes: attrs(a, of: n)), nil)
        }
    }

    func setAttributes(_ newAttributes: FSItem.SetAttributesRequest, on item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSSetAttributesResult?, (any Error)?) -> Void) {
        let body = { [self] in
            guard let h = v, let n = item as? Node else { return reply(nil, posix(.EIO)) }
            var (s, used) = setattr(newAttributes)
            var a = BfsAttr()
            let rc = withCaller(context) { who in withUnsafePointer(to: &s) { bfs_setattr(h, who, n.ino, $0, &a) } }
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            newAttributes.consumedAttributes = used
            reply(FSSetAttributesResult(attributes: attrs(a, of: n), freeSpace: nil), nil)
        }
        // Resizing a compressed file decompresses it first.
        if newAttributes.isValid(.size) { slow("setAttributes.size", body) } else { run("setAttributes", body) }
    }

    func enumerateDirectory(_ directory: FSItem, startingAt cookie: FSDirectoryCookie, verifier: FSDirectoryVerifier, attributes: FSItem.GetAttributesRequest?, packer: FSDirectoryEntryPacker, context: FSContext, replyHandler reply: @escaping @Sendable (FSEnumerateDirectoryResult?, (any Error)?) -> Void) {
        run("enumerateDirectory") {
            guard let h = v, let dir = directory as? Node else { return reply(nil, posix(.EIO)) }
            // FSKit adds "." and ".."; store cookies are stable and never
            // reused, so the verifier never changes.
            let box = DirPack(packer: packer, dir: dir.ino)
            let rc = withExtendedLifetime(box) {
                bfs_readdir(h, dir.ino, cookie.rawValue, attributes != nil, packEntry, Unmanaged.passUnretained(box).toOpaque())
            }
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            reply(FSEnumerateDirectoryResult(verifier: 1), nil)
        }
    }

    func readSymbolicLink(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSReadSymlinkResult?, (any Error)?) -> Void) {
        run("readSymbolicLink") {
            guard let h = v, let n = item as? Node else { return reply(nil, posix(.EIO)) }
            var b = BfsBuf()
            var a = BfsAttr()
            var rc = bfs_readlink(h, n.ino, &b)
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            let target = takeBuf(b)
            rc = bfs_getattr(h, n.ino, &a)
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            reply(FSReadSymlinkResult(contents: FSFileName(data: target), symlinkAttributes: attrs(a, of: n)), nil)
        }
    }

    // MARK: data

    func read(from item: FSItem, at offset: off_t, length: Int, into buffer: FSMutableFileDataBuffer, replyHandler reply: @escaping @Sendable (FSReadFileResult?, (any Error)?) -> Void) {
        // Chunked files may need decompression: keep it off the delivery queue.
        slow("read") { [self] in
            guard let h = v, let n = item as? Node, offset >= 0 else { return reply(nil, posix(.EIO)) }
            var got = 0
            let rc = buffer.withUnsafeMutableBytes { p in
                bfs_read(h, n.ino, UInt64(offset), p.bindMemory(to: UInt8.self).baseAddress, min(length, p.count), &got)
            }
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            var a = BfsAttr()
            let rc2 = bfs_getattr(h, n.ino, &a)
            guard rc2 == 0 else { return reply(nil, fsError(rc2)) }
            reply(FSReadFileResult(bytesRead: got, itemAttributes: attrs(a, of: n)), nil)
        }
    }

    func write(contents: Data, to item: FSItem, at offset: off_t, replyHandler reply: @escaping @Sendable (FSWriteFileResult?, (any Error)?) -> Void) {
        // Writing into a compressed file rehydrates it first.
        // Measured: inline writes are no faster (A/B, 2026-11-04), and
        // offloading keeps rare rehydrations off the delivery queue.
        slow("write") { [self] in
            guard let h = v, let n = item as? Node, offset >= 0 else { return reply(nil, posix(.EIO)) }
            var (done, a) = (0, BfsAttr())
            let rc = contents.withUnsafeBytes { p in
                bfs_write(h, n.ino, UInt64(offset), p.bindMemory(to: UInt8.self).baseAddress, p.count, &done, &a)
            }
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            reply(FSWriteFileResult(bytesWritten: done, itemAttributes: attrs(a, of: n), freeSpace: nil), nil)
        }
    }

    // MARK: kernel caching: buildfs is the only writer, so the kernel may
    // cache everything (write-back). Compression never changes visible
    // content, so no coherency actions are needed.

    func open(_ item: FSItem, modes: FSVolume.OpenModes, cacheMode: FSVolume.DataCacheMode, context: FSContext, replyHandler reply: @escaping @Sendable (FSOpenItemResult?, (any Error)?) -> Void) {
        run("open") {
            guard let h = v, let n = item as? Node else { return reply(nil, posix(.EIO)) }
            // close() comes once per item after the last reference, not once
            // per open, so the store sees one open state per item.
            let first = lock.withLockUnchecked { () -> Bool in defer { n.isOpen = true }; return !n.isOpen }
            if first { bfs_open_acquired(h, n.ino) }
            let granted: FSVolume.KernelCacheCoherencyType
            switch cacheMode {
            case .readWriteWithCache: granted = .writeBack
            case .readWithCache: granted = .readCache
            default: granted = .noCache
            }
            reply(FSOpenItemResult(grantedCoherency: granted), nil)
        }
    }

    func close(_ item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable () -> Void) {
        run("close") {
            guard let n = item as? Node else { return reply() }
            let was = lock.withLockUnchecked { () -> Bool in defer { n.isOpen = false }; return n.isOpen }
            if was, let h = v { bfs_open_released(h, n.ino) }
            reply()
        }
    }

    func upgrade(_ item: FSItem, cacheMode: FSVolume.DataCacheMode, context: FSContext, replyHandler reply: @escaping @Sendable (FSUpgradeItemResult?, (any Error)?) -> Void) {
        run("upgrade") {
            reply(FSUpgradeItemResult(grantedCoherency: cacheMode == .readWriteWithCache ? .writeBack : .readCache), nil)
        }
    }

    // MARK: xattrs: every name is stored, com.apple.* included (refusing
    // names breaks cp -p, ditto and fs::copy).

    func getXattr(named name: FSFileName, of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSGetXattrResult?, (any Error)?) -> Void) {
        run("getXattr") {
            guard let h = v, let n = item as? Node else { return reply(nil, posix(.EIO)) }
            var b = BfsBuf()
            let rc = withName(name) { bfs_get_xattr(h, n.ino, $0, $1, &b) }
            if trace { log.notice("trace getxattr \(name.string ?? "?", privacy: .public) of \(n.ino) rc=\(rc)") }
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            reply(FSGetXattrResult(xattrValue: takeBuf(b)), nil)
        }
    }

    func setXattr(named name: FSFileName, to value: Data?, on item: FSItem, policy: FSVolume.SetXattrPolicy, context: FSContext, replyHandler reply: @escaping @Sendable (FSSetXattrResult?, (any Error)?) -> Void) {
        run("setXattr") {
            guard let h = v, let n = item as? Node else { return reply(nil, posix(.EIO)) }
            let rc: Int32
            switch policy {
            case .delete:
                rc = withName(name) { bfs_remove_xattr(h, n.ino, $0, $1) }
            default:
                let mode = policy == .mustCreate ? BFS_XATTR_CREATE : policy == .mustReplace ? BFS_XATTR_REPLACE : BFS_XATTR_SET
                let data = value ?? Data()
                rc = withName(name) { np, nl in
                    data.withUnsafeBytes { bfs_set_xattr(h, n.ino, np, nl, $0.bindMemory(to: UInt8.self).baseAddress, $0.count, UInt32(mode)) }
                }
            }
            reply(rc == 0 ? FSSetXattrResult(freeSpace: nil) : nil, rc == 0 ? nil : fsError(rc))
        }
    }

    func listXattrs(of item: FSItem, context: FSContext, replyHandler reply: @escaping @Sendable (FSListXattrsResult?, (any Error)?) -> Void) {
        run("listXattrs") {
            guard let h = v, let n = item as? Node else { return reply(nil, posix(.EIO)) }
            var b = BfsBuf()
            let rc = bfs_list_xattrs(h, n.ino, &b)
            guard rc == 0 else { return reply(nil, fsError(rc)) }
            let names = takeBuf(b).split(separator: 0, omittingEmptySubsequences: true).map { FSFileName(data: Data($0)) }
            reply(FSListXattrsResult(xattrNames: names), nil)
        }
    }
}
