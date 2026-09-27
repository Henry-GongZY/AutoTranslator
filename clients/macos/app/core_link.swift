// translator-core supervision and the framed socket link.
//
// The client owns the core's lifecycle: spawn the engine binary, wait for its
// Unix socket, connect, and exchange length-prefixed Envelopes. Reads run on
// a dedicated queue and surface as LinkEvents; writes are serialised so audio
// chunks never interleave mid-frame.

import Foundation

enum CoreLinkError: Error, LocalizedError {
    case spawnFailed(String)
    case socketTimeout(String)
    case connectFailed(String)
    case ioClosed

    var errorDescription: String? {
        switch self {
        case .spawnFailed(let p): return "无法启动 translator-core: \(p)"
        case .socketTimeout(let p): return "core 未在 \(p) 上开始监听"
        case .connectFailed(let p): return "连接 core 失败: \(p)"
        case .ioClosed: return "与 core 的连接已断开"
        }
    }
}

final class CoreLink {
    enum LinkEvent {
        case connected(features: [String])
        case sessionStarted(ok: Bool, error: String)
        case sessionStopped(ok: Bool, error: String)
        case subtitle(SubtitleInfo)
        case status(code: Int, detail: String)
        case error(code: Int, message: String)
        case metrics(MetricsInfo)
        case disconnected
    }

    private(set) var process: Process?
    private var fd: Int32 = -1
    private var seq: UInt32 = 10
    private var socketPath = ""
    private var eventHandler: ((LinkEvent) -> Void)?
    private var readQueue: DispatchQueue?
    private let writeQueue = DispatchQueue(label: "autotranslator.core.write")
    private let stateLock = NSLock()
    private(set) var sessionId = ""

    /// Spawn the core, wait for its socket, connect and start reading.
    func launch(binary: String, socketPath: String, handler: @escaping (LinkEvent) -> Void) throws {
        self.socketPath = socketPath
        self.eventHandler = handler

        let process = Process()
        process.executableURL = URL(fileURLWithPath: binary)
        let logLevel = ProcessInfo.processInfo.environment["CORE_LOG"] ?? "info"
        process.arguments = ["--socket", socketPath, "--log-level", logLevel]
        if let stderr = FileHandle(forWritingAtPath: "/tmp/translator-core-mac.log") {
            stderr.seekToEndOfFile()
            process.standardError = stderr
        }
        try process.run()
        self.process = process

        // Wait until the core is listening (it removes stale sockets on bind).
        let deadline = Date().addingTimeInterval(15)
        while !FileManager.default.fileExists(atPath: socketPath) {
            if Date() > deadline {
                process.terminate()
                throw CoreLinkError.socketTimeout(socketPath)
            }
            Thread.sleep(forTimeInterval: 0.05)
        }

        fd = try connectSocket(path: socketPath)

        let queue = DispatchQueue(label: "autotranslator.core.read")
        readQueue = queue
        queue.async { [weak self] in
            self?.readLoop()
        }
    }

    func handshake() {
        print("[probe] handshake() called")
        sendRaw(Envelope.handshake())
    }

    func startSession(_ config: SessionConfig) {
        stateLock.lock()
        sessionId = config.sessionId
        stateLock.unlock()
        seq += 1
        sendRaw(Envelope.startSession(config))
    }

    /// Serialised on the write queue; never blocks the capture callback for
    /// more than one frame write. `rate`/`channels` must match the pcm layout.
    func sendAudio(rate: UInt32 = 48_000, channels: UInt32 = 2, frames: UInt32, timestampUs: UInt64, pcm: [UInt8]) {
        stateLock.lock()
        let id = sessionId
        stateLock.unlock()
        writeQueue.async { [weak self] in
            guard let self, self.fd >= 0 else { return }
            self.seq += 1
            let frame = Envelope.audioFrame(
                sessionId: id, timestampUs: timestampUs,
                rate: rate, channels: channels, frames: frames, pcm: pcm)
            guard writeAll(fd: self.fd, data: frame) else {
                self.emit(.disconnected)
                return
            }
        }
    }

    func setPaused(_ paused: Bool) {
        sendRaw(Envelope.setPaused(paused))
    }

    func stopSession() {
        stateLock.lock()
        let id = sessionId
        stateLock.unlock()
        sendRaw(Envelope.stopSession(sessionId: id))
    }

    func heartbeat() {
        let us = UInt64(Date().timeIntervalSince1970 * 1_000_000)
        sendRaw(Envelope.heartbeat(timestampUs: us))
    }

    /// Close the link; the core exits by itself once the controlling client
    /// disconnects (exit-on-disconnect), but terminate as belt and braces.
    func shutdown() {
        if fd >= 0 {
            close(fd)
            fd = -1
        }
        if let process, process.isRunning {
            process.terminate()
        }
        process = nil
    }

    // --- internals ---

    private func connectSocket(path: String) throws -> Int32 {
        guard path.utf8.count < 104 else {
            throw CoreLinkError.connectFailed("socket path too long: \(path)")
        }
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw CoreLinkError.connectFailed("socket() errno \(errno)") }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { dest in
            dest.copyBytes(from: Array(path.utf8))
        }
        let result = withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                connect(fd, sa, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard result == 0 else {
            let err = errno
            close(fd)
            throw CoreLinkError.connectFailed("errno \(err)")
        }
        return fd
    }

    private func sendRaw(_ frame: [UInt8]) {
        print("[probe] sendRaw \(frame.count)B")
        writeQueue.async { [weak self] in
            guard let self, self.fd >= 0 else { return }
            guard writeAll(fd: self.fd, data: frame) else {
                self.emit(.disconnected)
                return
            }
        }
    }

    private func readLoop() {
        while true {
            guard let header = readAll(fd: fd, count: 4) else {
                emit(.disconnected)
                return
            }
            let length = Int(header[0]) << 24 | Int(header[1]) << 16 | Int(header[2]) << 8 | Int(header[3])
            guard length > 0, length <= 4 * 1024 * 1024 else {
                emit(.disconnected)
                return
            }
            guard let payload = readAll(fd: fd, count: length) else {
                emit(.disconnected)
                return
            }
            let decoded = Envelope.decode(payload)
            print("[probe] rx \(length)B decoded=\(decoded != nil)")
            if let message = decoded {
                route(message)
            }
        }
    }

    private func route(_ message: IncomingMessage) {
        switch message {
        case .handshake(let accepted, let features, let error):
            emit(accepted ? .connected(features: features) : .error(code: 3, message: error))
        case .startSession(let accepted, _, let error):
            emit(.sessionStarted(ok: accepted, error: error.isEmpty ? (accepted ? "" : "session rejected") : error))
        case .stopSession(let ok, let error):
            emit(.sessionStopped(ok: ok, error: error))
        case .subtitle(let info):
            emit(.subtitle(info))
        case .status(let code, let detail):
            emit(.status(code: code, detail: detail))
        case .error(let code, let message):
            emit(.error(code: code, message: message))
        case .metrics(let m):
            emit(.metrics(m))
        case .heartbeat:
            break
        case .setPaused:
            break
        }
    }

    private func emit(_ event: LinkEvent) {
        eventHandler?(event)
    }
}

// --- POSIX helpers ---

extension CoreLink.LinkEvent: CustomStringConvertible {
    var description: String {
        switch self {
        case .connected(let f): return "connected(features=\(f))"
        case .sessionStarted(let ok, let e): return "sessionStarted(ok=\(ok) error=\(e))"
        case .sessionStopped(let ok, let e): return "sessionStopped(ok=\(ok) error=\(e))"
        case .subtitle(let s): return "subtitle(kind=\(s.kind) text=\(s.text) tr=\(s.translated))"
        case .status(let c, let d): return "status(\(c), \(d))"
        case .error(let c, let m): return "error(\(c), \(m))"
        case .metrics(let m): return "metrics(\(m.audioMs)ms dropped=\(m.droppedFrames))"
        case .disconnected: return "disconnected"
        }
    }
}

private func readAll(fd: Int32, count: Int) -> [UInt8]? {
    guard fd >= 0, count > 0 else { return nil }
    var buffer = [UInt8](repeating: 0, count: count)
    var got = 0
    while got < count {
        let n = buffer.withUnsafeMutableBytes { ptr -> Int in
            read(fd, ptr.baseAddress!.advanced(by: got), count - got)
        }
        if n <= 0 { return nil }
        got += n
    }
    return buffer
}

@discardableResult
func writeAll(fd: Int32, data: [UInt8]) -> Bool {
    guard fd >= 0 else { return false }
    return data.withUnsafeBytes { ptr -> Bool in
        var sent = 0
        while sent < data.count {
            let n = write(fd, ptr.baseAddress!.advanced(by: sent), data.count - sent)
            if n <= 0 { return false }
            sent += n
        }
        return true
    }
}
