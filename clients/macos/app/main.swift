// Headless self-test for the macOS client plumbing (no GUI, no TCC prompts):
//   1. protobuf codec round-trips
//   2. full core session over the real socket: spawn engine → handshake →
//      start (mock ASR + mock translation) → feed audio → collect committed
//      subtitles with translations → stop
//
// Build: scripts/build-macos-app.sh → target/core-selftest
// Requires the Metal engine package: engines/metal/translator-core.

import Foundation

enum SelfTest {
    static func run() {
        signal(SIGPIPE, SIG_IGN)
        var failures = 0

        failures += codecRoundTrip()
        failures += coreSessionTest()

        if failures == 0 {
            print("[self-test] ALL PASS")
            exit(0)
        }
        print("[self-test] \(failures) FAILURES")
        exit(1)
    }

    // MARK: codec

    private static func codecRoundTrip() -> Int {
        var failures = 0
        let cfg = SessionConfig(
            sessionId: "round-trip",
            asrProvider: "whisper",
            inputRate: 48_000,
            inputChannels: 2,
            sourceLanguage: "en",
            targetLanguage: "zh-Hans",
            translationProvider: "apple-translate",
            model: "tiny"
        )
        let frame = Envelope.startSession(cfg)

        // The decoder targets core→client messages; for the request side we
        // re-parse the raw envelope structure to assert field placement.
        var reader = ProtoReader(Array(frame[4...])) // skip length prefix
        var sawSeq = false, sawPayload = false
        while let f = reader.next() {
            if f.field == 1 { sawSeq = f.varint == 2 }
            if f.field == 4 {
                sawPayload = true
                var body = ProtoReader(f.bytes)
                var sessionId = "", asrLanguage = "", translation: [UInt8] = []
                var inputRate: UInt64 = 0
                while let g = body.next() {
                    switch g.field {
                    case 1: sessionId = g.text
                    case 2:
                        var fmt = ProtoReader(g.bytes)
                        while let h = fmt.next() {
                            if h.field == 1 { inputRate = h.varint }
                        }
                    case 4:
                        var asr = ProtoReader(g.bytes)
                        while let h = asr.next() {
                            if h.field == 3 { asrLanguage = h.text }
                        }
                    case 5: translation = g.bytes
                    default: break
                    }
                }
                if sessionId != "round-trip" || asrLanguage != "en" || inputRate != 48_000 || translation.isEmpty {
                    print("[self-test] FAIL start-session fields: id=\(sessionId) lang=\(asrLanguage) rate=\(inputRate) tr=\(translation.count)")
                    failures += 1
                }
            }
        }
        if !sawSeq || !sawPayload {
            print("[self-test] FAIL envelope structure seq=\(sawSeq) payload=\(sawPayload)")
            failures += 1
        }

        // Subtitle decode round-trip via a hand-built committed event.
        var subtitleBody = ProtoWire.uint64(3, 2)
        subtitleBody += ProtoWire.string(4, "hello")
        subtitleBody += ProtoWire.string(5, "你好")
        subtitleBody += ProtoWire.message(10, ProtoWire.string(1, "line"))
        let envelope = ProtoWire.uint64(1, 7) + ProtoWire.message(11, subtitleBody)
        guard case .subtitle(let info) = Envelope.decode(envelope) else {
            print("[self-test] FAIL subtitle decode")
            failures += 1
            return failures
        }
        if !info.isCommitted || info.text != "hello" || info.translated != "你好" {
            print("[self-test] FAIL subtitle fields: \(info)")
            failures += 1
        }
        print("[self-test] codec round-trip \(failures == 0 ? "OK" : "FAILED")")
        return failures
    }

    // MARK: core session

    private static func resolveCoreBinary() -> String {
        if let env = ProcessInfo.processInfo.environment["TRANSLATOR_CORE_BIN"], !env.isEmpty {
            return env
        }
        if let bundled = Bundle.main.path(forResource: "translator-core", ofType: nil) {
            return bundled
        }
        return "engines/metal/translator-core"
    }

    private static func coreSessionTest() -> Int {
        var failures = 0
        let core = resolveCoreBinary()
        guard FileManager.default.fileExists(atPath: core) else {
            print("[self-test] FAIL core binary not found at \(core) (run scripts/build-engines.sh metal)")
            return 1
        }

        let socketPath = "/tmp/translator-selftest-\(Int.random(in: 1000...9999)).sock"
        let link = CoreLink()
        let done = DispatchSemaphore(value: 0)
        let lock = NSLock()
        var committed = 0
        var sawMetal = false
        var lastError = ""

        do {
            try link.launch(binary: core, socketPath: socketPath) { event in
                switch event {
                case .connected(let features):
                    print("[selftest-event] connected, features: \(features)")
                    sawMetal = features.contains("asr.whisper.engine.metal")
                    // Mock ASR keeps this test free of model downloads; the
                    // Metal engine is still asserted via handshake features.
                    link.startSession(SessionConfig(
                        sessionId: "selftest",
                        asrProvider: "mock",
                        inputRate: 16_000,
                        inputChannels: 1,
                        sourceLanguage: "en",
                        targetLanguage: "zh-Hans",
                        translationProvider: "mock",
                        model: "tiny"
                    ))
                case .sessionStarted(let ok, let error):
                    print("[selftest-event] sessionStarted ok=\(ok) error=\(error)")
                    guard ok else {
                        lock.lock(); lastError = error; lock.unlock()
                        done.signal()
                        return
                    }
                    feedUtterances(link)
                case .subtitle(let info):
                    if info.isCommitted {
                        print("[selftest-event] committed: \(info.text) -> \(info.translated)")
                    }
                    if info.isCommitted && !info.translated.isEmpty {
                        lock.lock(); committed += 1; lock.unlock()
                        if committed >= 2 {
                            link.stopSession()
                        }
                    }
                case .sessionStopped:
                    done.signal()
                case .error(_, let message):
                    print("[selftest-event] error: \(message)")
                    lock.lock(); lastError = message; lock.unlock()
                case .disconnected:
                    lock.lock(); lastError = "disconnected"; lock.unlock()
                    done.signal()
                case .status, .metrics:
                    break
                }
            }
        } catch {
            print("[self-test] FAIL core launch: \(error)")
            return 1
        }
        link.handshake()

        guard done.wait(timeout: .now() + 60) == .success else {
            print("[self-test] FAIL core session timed out")
            link.shutdown()
            return 1
        }
        link.shutdown()

        lock.lock(); let c = committed, err = lastError; lock.unlock()
        if !sawMetal {
            print("[self-test] FAIL engine is not the Metal build (handshake features)")
            failures += 1
        }
        if c < 2 {
            print("[self-test] FAIL expected 2 committed translated subtitles, got \(c) (error: \(err))")
            failures += 1
        }
        if failures == 0 {
            print("[self-test] core session OK (metal engine, 2 committed translations)")
        }
        return failures
    }

    /// Two utterances (1.2 s tone + 0.8 s silence) as 16 kHz mono f32 — the
    /// core accepts any declared format; 16 kHz mono skips resampling. The
    /// silence is 0.8 s so the VAD gate (300 ms) closes and mock commits.
    private static func feedUtterances(_ link: CoreLink) {
        DispatchQueue.global().async {
            let rate = 16_000
            let chunkFrames = 1_600 // 100 ms
            for (seconds, loud) in [(1.2, true), (0.8, false), (1.2, true), (0.8, false)] {
                let count = Int(Double(rate) * seconds)
                var samples: [Float] = []
                samples.reserveCapacity(count)
                for i in 0..<count {
                    samples.append(loud ? 0.4 * sin(2 * Float.pi * 440 * Float(i) / Float(rate)) : 0.00001)
                }
                for chunk in strideChunks(samples, chunkFrames) {
                    var pcm: [UInt8] = []
                    pcm.reserveCapacity(chunk.count * 4)
                    for value in chunk {
                        withUnsafeBytes(of: value.bitPattern.littleEndian) { pcm.append(contentsOf: $0) }
                    }
                    link.sendAudio(rate: UInt32(rate), channels: 1, frames: UInt32(chunk.count), timestampUs: 0, pcm: pcm)
                    Thread.sleep(forTimeInterval: 0.05)
                }
            }
        }
    }

    private static func strideChunks(_ samples: [Float], _ size: Int) -> [[Float]] {
        stride(from: 0, to: samples.count, by: size).map { Array(samples[$0..<min($0 + size, samples.count)]) }
    }
}

// Top-level entry for the headless self-test binary.
signal(SIGPIPE, SIG_IGN)
SelfTest.run()
