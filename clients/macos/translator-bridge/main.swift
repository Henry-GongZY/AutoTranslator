// translator-bridge — macOS helper process that exposes the Apple Translation
// framework over a local Unix socket with a JSON line protocol.
//
// The Rust core (provider `apple-translate`) connects and speaks:
//
//   -> {"id":1,"op":"capabilities","source":"en","target":"zh-Hans"}
//   <- {"id":1,"ok":true,"system_version":"15.7","status":"installed"}
//   -> {"id":2,"op":"translate","text":"Hello","source":"en","target":"zh-Hans"}
//   <- {"id":2,"ok":true,"text":"你好"}
//
// Session acquisition:
//   * macOS 26+: direct `TranslationSession(installedSource:target:)` — no UI
//     required, but the source language must be installed on the system.
//   * macOS 15–25: `TranslationSession` is only obtainable through SwiftUI's
//     `.translationTask(_:_:)` modifier, so we host a 1x1 offscreen window
//     (the approach proven by open-source macOS translators).
//
// The build SDK must be macOS 26+ for the direct path to compile; the runtime
// picks the branch via #available. Status values follow
// `LanguageAvailability.status(from:to:)`: installed | supported | unsupported.
// `supported` means the pair is translatable but assets may still download on
// first use — a failed translate surfaces that to the core as an error and the
// subtitle ships untranslated.

import AppKit
import AVFoundation
import Foundation
import Speech
import SwiftUI
import Translation

struct BridgeRequest: Codable {
    let id: Int
    let op: String
    let text: String?
    let source: String?
    let target: String?
}

struct BridgeResponse: Codable {
    let id: Int
    let ok: Bool
    let text: String?
    let error: String?
    let system_version: String?
    let status: String?
    let capabilities: [String: JSONValue]?

    init(id: Int, ok: Bool, text: String? = nil, error: String? = nil,
         system_version: String? = nil, status: String? = nil,
         capabilities: [String: JSONValue]? = nil) {
        self.id = id
        self.ok = ok
        self.text = text
        self.error = error
        self.system_version = system_version
        self.status = status
        self.capabilities = capabilities
    }
}

/// Type-erased JSON value so capabilities can carry arrays of strings/bools.
enum JSONValue: Codable {
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])

    init(_ any: Any) {
        switch any {
        case let v as Bool: self = .bool(v)
        case let v as Double: self = .number(v)
        case let v as Int: self = .number(Double(v))
        case let v as String: self = .string(v)
        case let v as [Any]: self = .array(v.map(JSONValue.init))
        default: self = .string(String(describing: any))
        }
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if let v = try? container.decode(Bool.self) { self = .bool(v) }
        else if let v = try? container.decode(Double.self) { self = .number(v) }
        else if let v = try? container.decode(String.self) { self = .string(v) }
        else if let v = try? container.decode([JSONValue].self) { self = .array(v) }
        else { throw DecodingError.typeMismatch(JSONValue.self, DecodingError.Context(codingPath: decoder.codingPath, debugDescription: "unsupported value")) }
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .bool(let v): try container.encode(v)
        case .number(let v): try container.encode(v)
        case .string(let v): try container.encode(v)
        case .array(let v): try container.encode(v)
        }
    }
}

enum BridgeError: Error, LocalizedError {
    case cancelled
    case assetsNotInstalled(String)
    case requiresMacOS26
    case speechUnavailable
    case speechLocaleUnsupported(String)
    case speechFormatUnsupported(String)
    case asrNotStarted

    var errorDescription: String? {
        switch self {
        case .cancelled:
            return "request superseded"
        case .assetsNotInstalled(let pair):
            return "language assets for \(pair) are not installed; trigger the download once from any app that offers system translation (e.g. Safari's translate button), then retry"
        case .requiresMacOS26:
            return "Apple SpeechAnalyzer requires macOS 26 or newer; use the whisper engine on this system"
        case .speechUnavailable:
            return "SpeechTranscriber is not available on this system"
        case .speechLocaleUnsupported(let detail):
            return "SpeechTranscriber does not support this locale: \(detail)"
        case .speechFormatUnsupported(let detail):
            return "unsupported audio format from SpeechAnalyzer: \(detail)"
        case .asrNotStarted:
            return "streaming recognition has not been started"
        }
    }
}

/// Owns the current `TranslationSession`. One session per language pair is
/// kept alive and reused; changing the pair (or a failed translate) forces a
/// fresh acquisition.
@MainActor
final class TranslationService: ObservableObject {
    @Published private(set) var configuration: TranslationSession.Configuration?
    private var waiter: CheckedContinuation<TranslationSession, Error>?
    private var pendingPair: (source: String, target: String)?
    private var cached: (source: String, target: String, session: TranslationSession)?

    /// Unused while the bridge fails fast on uninstalled assets; kept for the
    /// macOS 26 direct-init path where downloads can be requested in-process.
    var presentHost: (() -> Void)?
    var hideHost: (() -> Void)?

    func translate(text: String, source: String, target: String) async throws -> String {
        log("translate start pair=\(source)->\(target) chars=\(text.count)")
        let session = try await acquireSession(source: source, target: target)
        do {
            let response = try await session.translate(text)
            log("translate ok")
            return response.targetText
        } catch {
            // macOS 26 can drive the language-asset download from inside this
            // helper process; prepare then retry once before giving up.
            if #available(macOS 26.0, *), session.canRequestDownloads {
                log("asset download possible in-process, preparing translation")
                do {
                    try await session.prepareTranslation()
                    let response = try await session.translate(text)
                    log("translate ok after prepare")
                    return response.targetText
                } catch {
                    cached = nil
                    log("translate failed after prepare: \(error)")
                    throw error
                }
            }
            // The session may have gone stale (assets changed, app state);
            // force a fresh acquisition on the next request.
            cached = nil
            log("translate failed: \(error)")
            throw error
        }
    }

    private func acquireSession(source: String, target: String) async throws -> TranslationSession {
        if let c = cached, c.source == source, c.target == target {
            return c.session
        }
        if #available(macOS 26.0, *) {
            log("acquire via direct init (macOS 26 path)")
            let session = TranslationSession(
                installedSource: Locale.Language(identifier: source),
                target: Locale.Language(identifier: target)
            )
            cached = (source, target, session)
            return session
        }

        // macOS 15 path: obtain the session through the hidden SwiftUI host.
        // Asset state decides up front: `unsupported` pairs can never work,
        // and `supported` (not-yet-installed) pairs cannot download from a
        // helper process — `prepareTranslation` blocks forever without ever
        // presenting its confirmation here. Fail fast with guidance; the
        // download interaction belongs to the macOS client (install the pair
        // via any app that offers system translation, e.g. Safari).
        let pair = "\(source)→\(target)"
        let availability = LanguageAvailability()
        let status = await availability.status(
            from: Locale.Language(identifier: source),
            to: Locale.Language(identifier: target)
        )
        guard status == .installed else {
            log("pair \(pair) status \(status) — assets not installed, failing fast")
            throw BridgeError.assetsNotInstalled(pair)
        }
        log("pair \(pair) installed, acquiring session")

        if let pending = waiter {
            pending.resume(throwing: BridgeError.cancelled)
            waiter = nil
            pendingPair = nil
        }
        log("acquire via translationTask (macOS 15 path)")
        let session: TranslationSession = try await withCheckedThrowingContinuation { cont in
            self.waiter = cont
            self.pendingPair = (source, target)
            self.configuration = TranslationSession.Configuration(
                source: Locale.Language(identifier: source),
                target: Locale.Language(identifier: target)
            )
            // SwiftUI re-fires translationTask on the next runloop pass and
            // calls deliver(_:), which resumes this continuation.
        }

        cached = (source, target, session)
        return session
    }

    func deliver(_ session: TranslationSession) {
        log("translationTask delivered a session")
        guard let waiter else { return }
        self.waiter = nil
        if let pair = pendingPair {
            cached = (pair.source, pair.target, session)
            pendingPair = nil
        }
        waiter.resume(returning: session)
    }
}

struct BridgeRootView: View {
    @ObservedObject var service: TranslationService

    var body: some View {
        Color.clear
            .frame(width: 1, height: 1)
            .translationTask(service.configuration) { session in
                service.deliver(session)
            }
    }
}

func log(_ message: String) {
    FileHandle.standardError.write(Data("[bridge] \(message)\n".utf8))
}

func pairStatus(source: String, target: String) async -> String {
    let availability = LanguageAvailability()
    let status = await availability.status(
        from: Locale.Language(identifier: source),
        to: Locale.Language(identifier: target)
    )
    switch status {
    case .installed: return "installed"
    case .supported: return "supported"
    case .unsupported: return "unsupported"
    @unknown default: return "unknown"
    }
}

func systemVersion() -> String {
    let v = ProcessInfo.processInfo.operatingSystemVersion
    return "\(v.majorVersion).\(v.minorVersion)"
}

func encode(_ response: BridgeResponse) -> String {
    let data = try? JSONEncoder().encode(response)
    return data.flatMap { String(data: $0, encoding: .utf8) }
        ?? #"{"id":0,"ok":false,"error":"encode failed"}"#
}

func process(req: BridgeRequest, service: TranslationService, asr: SpeechRecognitionService, pending: PendingLines?) async -> BridgeResponse {
    switch req.op {
    case "capabilities":
        var status: String? = nil
        if let s = req.source, let t = req.target {
            status = await pairStatus(source: s, target: t)
        }
        return BridgeResponse(id: req.id, ok: true, system_version: systemVersion(), status: status)

    case "asr-capabilities":
        let caps = await asr.capabilities()
        return BridgeResponse(
            id: req.id, ok: true,
            system_version: systemVersion(),
            status: nil,
            capabilities: caps.mapValues(JSONValue.init))

    case "asr-start":
        guard let locale = req.source, let pending = pending else {
            return BridgeResponse(id: req.id, ok: false, error: "asr-start requires source (locale)")
        }
        do {
            try await asr.start(locale: locale) { line in pending.append(line) }
            return BridgeResponse(id: req.id, ok: true, status: String(asr.sampleRate))
        } catch {
            return BridgeResponse(id: req.id, ok: false, error: error.localizedDescription)
        }

    case "asr-feed":
        guard let b64 = req.text, let bytes = Data(base64Encoded: b64), let pending = pending else {
            return BridgeResponse(id: req.id, ok: false, error: "asr-feed requires base64 pcm")
        }
        do {
            let samples = bytes.withUnsafeBytes { raw in
                stride(from: 0, to: raw.count, by: 4).map {
                    Float(bitPattern: raw.loadUnaligned(fromByteOffset: $0, as: UInt32.self))
                }
            }
            try asr.feed(samples: samples)
            return BridgeResponse(id: req.id, ok: true)
        } catch {
            return BridgeResponse(id: req.id, ok: false, error: error.localizedDescription)
        }

    case "asr-stop":
        do {
            try await asr.stop()
            return BridgeResponse(id: req.id, ok: true)
        } catch {
            return BridgeResponse(id: req.id, ok: false, error: error.localizedDescription)
        }

    case "translate":
        guard let text = req.text, let source = req.source, let target = req.target else {
            return BridgeResponse(id: req.id, ok: false,
                                  error: "translate requires text, source and target")
        }
        do {
            let translated = try await service.translate(text: text, source: source, target: target)
            return BridgeResponse(id: req.id, ok: true, text: translated)
        } catch {
            return BridgeResponse(id: req.id, ok: false,
                                  error: "translation failed: \(error.localizedDescription)")
        }

    default:
        return BridgeResponse(id: req.id, ok: false, error: "unknown op `\(req.op)`")
    }
}

/// Bridge one async request to the synchronous socket loop.
func respondSync(line: String, service: TranslationService, asr: SpeechRecognitionService, pending: PendingLines) -> String {
    guard let data = line.data(using: .utf8),
          let req = try? JSONDecoder().decode(BridgeRequest.self, from: data) else {
        return encode(BridgeResponse(id: 0, ok: false, error: "bad request JSON"))
    }
    let semaphore = DispatchSemaphore(value: 0)
    var payload = ""
    Task.detached {
        let response = await process(req: req, service: service, asr: asr, pending: pending)
        payload = encode(response)
        semaphore.signal()
    }
    // Long enough to cover a first-use language asset download.
    if semaphore.wait(timeout: .now() + 150) == .timedOut {
        return encode(BridgeResponse(id: req.id, ok: false,
                                     error: "request timed out inside the bridge"))
    }
    return payload
}

func serveConnection(fd: Int32, service: TranslationService, queue: DispatchQueue) {
    queue.async {
        // Per-connection streaming-ASR state and its async event queue.
        let asr = SpeechRecognitionService()
        let pending = PendingLines()

        func writeAll(_ text: String) -> Bool {
            let out = Data(text.utf8)
            return out.withUnsafeBytes { ptr -> Bool in
                write(fd, ptr.baseAddress, ptr.count) == out.count
            }
        }

        var buffer = Data()
        let newline = Data("\n".utf8)
        var chunk = [UInt8](repeating: 0, count: 1 << 16)
        loop: while true {
            let n = read(fd, &chunk, chunk.count)
            guard n > 0 else { break }
            buffer.append(contentsOf: chunk[0..<n])
            while let range = buffer.firstRange(of: newline) {
                let lineData = buffer.subdata(in: buffer.startIndex..<range.lowerBound)
                buffer.removeSubrange(buffer.startIndex..<range.upperBound)
                guard let line = String(data: lineData, encoding: .utf8),
                      !line.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { continue }
                let response = respondSync(line: line, service: service, asr: asr, pending: pending) + "\n"
                if !writeAll(response) { break loop }
                // Recognition events arrive out of band; flush them right
                // after each reply so partials keep flowing while feeding.
                for event in pending.drain() {
                    if !writeAll(event + "\n") { break loop }
                }
            }
        }
        close(fd)
    }
}

func startSocketServer(path: String, service: TranslationService) {
    try? FileManager.default.removeItem(atPath: path)

    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    guard fd >= 0 else { fatalError("socket() failed, errno \(errno)") }
    guard path.utf8.count < 104 else { fatalError("socket path too long: \(path)") }

    var addr = sockaddr_un()
    addr.sun_family = sa_family_t(AF_UNIX)
    withUnsafeMutableBytes(of: &addr.sun_path) { dest in
        dest.copyBytes(from: Array(path.utf8))
    }
    let bindResult = withUnsafePointer(to: &addr) { ptr in
        ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
            bind(fd, sa, socklen_t(MemoryLayout<sockaddr_un>.size))
        }
    }
    guard bindResult == 0 else { fatalError("bind(\(path)) failed, errno \(errno)") }
    chmod(path, 0o600)
    guard listen(fd, 8) == 0 else { fatalError("listen() failed, errno \(errno)") }

    let acceptQueue = DispatchQueue(label: "translator-bridge.accept")
    let connectionQueue = DispatchQueue(label: "translator-bridge.connection", attributes: .concurrent)
    let source = DispatchSource.makeReadSource(fileDescriptor: fd, queue: acceptQueue)
    source.setEventHandler {
        while true {
            let client = accept(fd, nil, nil)
            guard client >= 0 else { break }
            serveConnection(fd: client, service: service, queue: connectionQueue)
        }
    }
    source.resume()
    keepAliveSource = source

    FileHandle.standardError.write(Data("translator-bridge listening on \(path)\n".utf8))
}

// --- streaming ASR (SpeechAnalyzer, macOS 26+) --------------------------------

/// Thread-safe queue of asynchronously emitted event lines (recognition
/// results arrive on their own task, not in reply to a request).
final class PendingLines {
    private let lock = NSLock()
    private var lines: [String] = []

    func append(_ line: String) {
        lock.lock()
        lines.append(line)
        lock.unlock()
    }

    func drain() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        let out = lines
        lines = []
        return out
    }
}

/// Public facade: safe to touch on any OS; the real work is 26-gated.
final class SpeechRecognitionService {
    private var impl: Any?
    private(set) var sampleRate = 16_000.0
    private(set) var active = false

    func capabilities() async -> [String: Any] {
        var payload: [String: Any] = ["speech_available": false]
        if #available(macOS 26.0, *) {
            payload["speech_available"] = SpeechTranscriber.isAvailable
            payload["supported_locales"] = await SpeechTranscriber.supportedLocales.map { $0.identifier(.bcp47) }
            payload["installed_locales"] = await SpeechTranscriber.installedLocales.map { $0.identifier(.bcp47) }
        }
        return payload
    }

    func start(locale: String, emit: @escaping (String) -> Void) async throws {
        guard #available(macOS 26.0, *) else {
            throw BridgeError.requiresMacOS26
        }
        guard SpeechTranscriber.isAvailable else {
            throw BridgeError.speechUnavailable
        }
        let requested = Locale(identifier: locale)
        guard let supported = await SpeechTranscriber.supportedLocale(equivalentTo: requested) else {
            let locales = await SpeechTranscriber.supportedLocales.map { $0.identifier(.bcp47) }
            throw BridgeError.speechLocaleUnsupported("\(locale) (supported: \(locales.prefix(8).joined(separator: ", ")))")
        }
        let impl = try await SpeechTranscriberServiceImpl(locale: supported, emit: emit)
        sampleRate = impl.sampleRate
        self.impl = impl
        active = true
    }

    func feed(samples: [Float]) throws {
        guard #available(macOS 26.0, *), let impl = impl as? SpeechTranscriberServiceImpl else {
            throw BridgeError.requiresMacOS26
        }
        try impl.feed(samples: samples)
    }

    func stop() async throws {
        guard #available(macOS 26.0, *), let impl = impl as? SpeechTranscriberServiceImpl else {
            throw BridgeError.requiresMacOS26
        }
        try await impl.stop()
        active = false
    }
}

@available(macOS 26.0, *)
private final class SpeechTranscriberServiceImpl {
    private let transcriber: SpeechTranscriber
    private let analyzer: SpeechAnalyzer
    private let input: AsyncStream<AnalyzerInput>.Continuation
    private let analyzerFormat: AVAudioFormat
    private var pump: Task<Void, Never>?
    /// Media clock in SAMPLES (timescale = sample rate): microsecond
    /// accumulation quantizes against the 16 kHz sample grid and makes the
    /// next buffer's start land one sample before the prior end (SFSpeechError
    /// 17, "overlaps or precedes"), triggered by warm-up chunks like 795
    /// samples = 49687.5 µs.
    private var mediaSamples: Int64 = 0
    let sampleRate: Double

    init(locale: Locale, emit: @escaping (String) -> Void) async throws {
        let transcriber = SpeechTranscriber(locale: locale, preset: .progressiveTranscription)
        let (stream, continuation) = AsyncStream.makeStream(of: AnalyzerInput.self, bufferingPolicy: .unbounded)
        input = continuation

        let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber])
        guard let format else { throw BridgeError.speechUnavailable }
        // Contract: mono PCM (what the core feeds). SpeechAnalyzer on macOS
        // 26/27 reports 16 kHz Int16; we convert from the core's f32 in feed.
        guard format.channelCount == 1,
              format.commonFormat == .pcmFormatFloat32 || format.commonFormat == .pcmFormatInt16 else {
            throw BridgeError.speechFormatUnsupported("\(format)")
        }
        sampleRate = format.sampleRate

        analyzer = SpeechAnalyzer(modules: [transcriber])
        self.transcriber = transcriber
        self.analyzerFormat = format
        try await analyzer.start(inputSequence: stream)

        pump = Task { [weak self] in
            do {
                for try await result in transcriber.results {
                    guard let self else { break }
                    let text = String(result.text.characters)
                    let startUs = UInt64(max(0, result.range.start.seconds * 1_000_000))
                    let endUs = startUs + UInt64(max(0, result.range.duration.seconds * 1_000_000))

                    let payload: [String: Any] = [
                        "event": "asr", "text": text,
                        "final": result.isFinal,
                        "start_us": startUs, "end_us": endUs,
                    ]
                    if let data = try? JSONSerialization.data(withJSONObject: payload),
                       let line = String(data: data, encoding: .utf8) {
                        emit(line)
                    }
                }
            } catch {
                log("asr results stream failed: \(error)")
            }
        }
    }

    private var feedLogCount = 0

    func feed(samples: [Float]) throws {
        if feedLogCount < 12 {
            feedLogCount += 1
            log("feed #\(feedLogCount): samples=\(samples.count) mediaSamples=\(mediaSamples)")
        }
        let frames = AVAudioFrameCount(samples.count)
        guard let buffer = AVAudioPCMBuffer(pcmFormat: analyzerFormat, frameCapacity: frames) else {
            throw BridgeError.speechFormatUnsupported("cannot allocate buffer")
        }
        buffer.frameLength = frames
        if let channel = buffer.floatChannelData?[0] {
            for (i, sample) in samples.enumerated() { channel[i] = sample }
        } else if let channel = buffer.int16ChannelData?[0] {
            for (i, sample) in samples.enumerated() {
                channel[i] = Int16(clamping: Int32((sample * 32767.0).rounded()))
            }
        }
        let start = CMTime(value: CMTimeValue(mediaSamples), timescale: CMTimeScale(sampleRate))
        mediaSamples += Int64(frames)
        input.yield(AnalyzerInput(buffer: buffer, bufferStartTime: start))
    }

    func stop() async throws {
        input.finish()
        try await analyzer.finalizeAndFinishThroughEndOfInput()
        await pump?.value // trailing Final events are emitted before returning
    }
}

/// Keeps the accept dispatch source alive (top-level storage).
var keepAliveSource: DispatchSourceRead?

func parseArguments() -> String {
    var socketPath = "/tmp/translator-bridge-v1.sock"
    var args = Array(CommandLine.arguments.dropFirst())
    while !args.isEmpty {
        switch args[0] {
        case "--socket":
            guard args.count >= 2 else { fatalError("--socket needs a path") }
            socketPath = args[1]
            args.removeFirst(2)
        default:
            fatalError("unknown argument \(args[0])")
        }
    }
    return socketPath
}

// The whole setup must run on the main actor: AppKit windows and the SwiftUI
// host that hands out TranslationSessions on macOS 15 live there.
let socketPath = parseArguments()
MainActor.assumeIsolated {
    let app = NSApplication.shared
    // Session acquisition via translationTask works from a hidden window
    // (verified on macOS 15.7); the app never becomes visible.
    app.setActivationPolicy(.prohibited)

    let service = TranslationService()
    let window = NSWindow(
        contentRect: NSRect(x: 0, y: 0, width: 1, height: 1),
        styleMask: [.borderless],
        backing: .buffered,
        defer: false
    )
    window.contentView = NSHostingView(rootView: BridgeRootView(service: service))
    window.setFrameOrigin(NSPoint(x: -30000, y: -30000))
    window.orderFrontRegardless()

    service.presentHost = { window.orderFrontRegardless() }
    service.hideHost = { window.orderOut(nil) }

    startSocketServer(path: socketPath, service: service)
    app.run()
}
