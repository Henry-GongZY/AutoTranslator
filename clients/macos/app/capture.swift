// System-wide audio capture via ScreenCaptureKit.
//
// Chosen over Core Audio Process Taps for v1: one callback, whole-system
// audio, standard Screen Recording permission. Process Taps stay on the
// backlog for the skill's power-consumption comparison.
//
// ScreenCaptureKit delivers 48 kHz stereo Float32 (non-interleaved); we read
// the AudioBufferList directly, re-interleave into 50 ms chunks and hand them
// to the core link.

import AVFoundation
import CoreMedia
import ScreenCaptureKit

/// One-shot diagnostic logging (format description on first audio buffer).
private var loggedFormat = false
func logOnce(_ message: String) {
    guard !loggedFormat else { return }
    loggedFormat = true
    appLog("[capture] \(message)")
}

final class SystemAudioCapture: NSObject, SCStreamOutput {
    var onChunk: ((_ rate: UInt32, _ channels: UInt32, _ frames: UInt32, _ timestampUs: UInt64, _ pcm: [UInt8]) -> Void)?
    private(set) var droppedChunks = 0

    private var stream: SCStream?
    private var accum: [Float] = []
    private var accumFrames = 0
    private var sampleRate: Double = 48_000
    private var channels = 2
    private var timestampUs: UInt64 = 0
    /// ~50 ms at 48 kHz.
    private let chunkFrames = 2400
    private let lock = NSLock()
    /// Cap on queued float samples before new audio is dropped: realtime
    /// semantics beat a growing backlog (the skill's bounded-queue rule).
    private let maxAccumFrames = 48_000 * 4
    private let outputQueue = DispatchQueue(label: "autotranslator.sck-audio")

    static func hasPermission() -> Bool {
        CGPreflightScreenCaptureAccess()
    }

    /// Returns false when the user dismissed the prompt.
    static func requestPermission() -> Bool {
        CGRequestScreenCaptureAccess()
    }

    func start() async throws {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        guard let display = content.displays.first else {
            throw CaptureError.noDisplay
        }
        let filter = SCContentFilter(display: display, excludingWindows: [])
        let config = SCStreamConfiguration()
        config.capturesAudio = true
        config.sampleRate = 48_000
        config.channelCount = 2
        // Do not capture our own output (avoids the subtitle app feeding back).
        config.excludesCurrentProcessAudio = true
        // Audio-only in spirit: minimal, slow video we simply drop.
        config.width = 2
        config.height = 2
        config.minimumFrameInterval = CMTime(value: 1, timescale: 2)
        config.queueDepth = 8

        let stream = SCStream(filter: filter, configuration: config, delegate: nil)
        try stream.addStreamOutput(self, type: .audio, sampleHandlerQueue: outputQueue)
        do {
            try await stream.startCapture()
        } catch {
            appLog("[capture] startCapture FAILED: \(error)")
            throw error
        }
        appLog("[capture] started (audio=\(config.capturesAudio), \(config.sampleRate) Hz x \(config.channelCount))")
        self.stream = stream
    }

    func stop() {
        guard let stream else { return }
        self.stream = nil
        Task {
            try? await stream.stopCapture()
        }
        lock.lock()
        accum.removeAll()
        accumFrames = 0
        lock.unlock()
    }

    // SCStreamOutput
    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        callbackCount += 1
        if callbackCount % 100 == 1 {
            appLog("[capture] callback #\(callbackCount) type=\(type == .audio ? "audio" : "other")")
        }
        guard type == .audio else { return }
        ingest(sampleBuffer)
    }
    private var callbackCount = 0

    private func ingest(_ sampleBuffer: CMSampleBuffer) {
        guard CMSampleBufferDataIsReady(sampleBuffer) else { return drop("data not ready") }
        guard let formatDesc = CMSampleBufferGetFormatDescription(sampleBuffer),
              let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(formatDesc)?.pointee else {
            return drop("no format description")
        }

        // The list must be sized for every channel: AudioBufferList carries one
        // inline AudioBuffer, stereo non-interleaved needs one more behind it.
        var needed = 0
        var sizeStatus = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sampleBuffer,
            bufferListSizeNeededOut: &needed,
            bufferListOut: nil,
            bufferListSize: 0,
            blockBufferAllocator: kCFAllocatorDefault,
            blockBufferMemoryAllocator: kCFAllocatorDefault,
            flags: 0,
            blockBufferOut: nil
        )
        guard sizeStatus == noErr, needed > 0 else { return drop("size probe \(sizeStatus)") }
        if scratch.count < needed { scratch = [UInt8](repeating: 0, count: needed) }

        var blockBuffer: CMBlockBuffer?
        var extractStatus: OSStatus = -1
        scratch.withUnsafeMutableBytes { raw in
            let list = raw.baseAddress!.assumingMemoryBound(to: AudioBufferList.self)
            extractStatus = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
                sampleBuffer,
                bufferListSizeNeededOut: nil,
                bufferListOut: list,
                bufferListSize: needed,
                blockBufferAllocator: kCFAllocatorDefault,
                blockBufferMemoryAllocator: kCFAllocatorDefault,
                flags: kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
                blockBufferOut: &blockBuffer
            )
            guard extractStatus == noErr, blockBuffer != nil else { return }

            let buffers = UnsafeMutableAudioBufferListPointer(list)
            guard let first = buffers.first, first.mDataByteSize > 0 else { return }
            let channelsPerBuffer = Int(first.mNumberChannels)
            // Interleaved = one buffer carrying all channels; non-interleaved =
            // one AudioBuffer per channel. SCK has delivered both layouts.
            let interleaved = buffers.count == 1 && channelsPerBuffer > 1
            let frames = Int(first.mDataByteSize) / MemoryLayout<Float>.size / (interleaved ? channelsPerBuffer : 1)
            guard frames > 0 else { return }
            let ch = max(1, channelsPerBuffer * buffers.count)
            logOnce("audio format: \(asbd.mSampleRate) Hz x \(ch) ch, buffers=\(buffers.count) ch/buf=\(channelsPerBuffer) interleave=\(interleaved)")

            lock.lock()
            defer { lock.unlock() }
            if accumFrames + frames > maxAccumFrames {
                droppedChunks += 1
                return
            }
            accum.reserveCapacity((accumFrames + frames) * ch)
            if interleaved {
                let data = first.mData!.assumingMemoryBound(to: Float.self)
                for f in 0..<frames {
                    for c in 0..<ch {
                        accum.append(data[f * ch + c])
                    }
                }
            } else {
                for f in 0..<frames {
                    for c in 0..<ch {
                        let audioBuffer = buffers[c]
                        let data = audioBuffer.mData?.assumingMemoryBound(to: Float.self)
                        accum.append(data?[f] ?? 0)
                    }
                }
            }
            accumFrames += frames

            let stride = chunkFrames * ch
            while accumFrames >= chunkFrames {
                let slice = Array(accum.prefix(stride))
                accum.removeFirst(stride)
                accumFrames -= chunkFrames
                var bytes: [UInt8] = []
                bytes.reserveCapacity(slice.count * 4)
                for value in slice {
                    withUnsafeBytes(of: value.bitPattern.littleEndian) { bytes.append(contentsOf: $0) }
                }
                sampleRate = asbd.mSampleRate
                channels = ch
                let rate = UInt32(asbd.mSampleRate)
                let channelCount = UInt32(ch)
                timestampUs &+= UInt64(Double(chunkFrames) * 1_000_000 / max(1, asbd.mSampleRate))
                let ts = timestampUs
                // onChunk hands the chunk to the core link's write queue; safe
                // to call while holding our lock (it never blocks on us).
                onChunk?(rate, channelCount, UInt32(chunkFrames), ts, bytes)
            }
        }
        if extractStatus != noErr {
            drop("extract \(extractStatus)")
        }
    }

    /// Reason-coded drop counter so extraction failures are visible in the log
    /// without flooding it.
    private func drop(_ reason: String) {
        dropReasons[reason, default: 0] += 1
        if dropReasons[reason]! <= 3 {
            appLog("[capture] ingest drop: \(reason) (x\(dropReasons[reason]!))")
        }
    }
    private var dropReasons: [String: Int] = [:]
    /// Scratch storage for the AudioBufferList extraction.
    private var scratch: [UInt8] = []

    enum CaptureError: Error, LocalizedError {
        case noDisplay

        var errorDescription: String? { "没有可用的显示器用于采集会话" }
    }
}
