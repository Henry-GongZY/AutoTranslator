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
        try await stream.startCapture()
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
        guard type == .audio else { return }
        ingest(sampleBuffer)
    }

    private func ingest(_ sampleBuffer: CMSampleBuffer) {
        guard CMSampleBufferDataIsReady(sampleBuffer) else { return }
        guard let formatDesc = CMSampleBufferGetFormatDescription(sampleBuffer),
              let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(formatDesc)?.pointee else {
            return
        }

        var bufferList = AudioBufferList()
        var blockBuffer: CMBlockBuffer?
        let status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sampleBuffer,
            bufferListSizeNeededOut: nil,
            bufferListOut: &bufferList,
            bufferListSize: MemoryLayout<AudioBufferList>.size,
            blockBufferAllocator: kCFAllocatorDefault,
            blockBufferMemoryAllocator: kCFAllocatorDefault,
            flags: kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            blockBufferOut: &blockBuffer
        )
        guard status == noErr, blockBuffer != nil else { return }

        let buffers = UnsafeMutableAudioBufferListPointer(&bufferList)
        guard let first = buffers.first, first.mDataByteSize > 0 else { return }
        // Non-interleaved: one AudioBuffer per channel, frame count from bytes.
        let frames = Int(first.mDataByteSize) / MemoryLayout<Float>.size
        guard frames > 0 else { return }
        let ch = max(1, buffers.count)

        lock.lock()
        if accumFrames + frames > maxAccumFrames {
            droppedChunks += 1
            lock.unlock()
            return
        }
        accum.reserveCapacity((accumFrames + frames) * ch)
        for f in 0..<frames {
            for c in 0..<ch {
                let audioBuffer = buffers[c]
                let data = audioBuffer.mData?.assumingMemoryBound(to: Float.self)
                accum.append(data?[f] ?? 0)
            }
        }
        accumFrames += frames

        var chunks: [[UInt8]] = []
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
            chunks.append(bytes)
        }
        sampleRate = asbd.mSampleRate
        channels = ch
        lock.unlock()

        guard !chunks.isEmpty else { return }
        let rate = UInt32(asbd.mSampleRate)
        let channelCount = UInt32(ch)
        let usPerChunk = UInt64(Double(chunkFrames) * 1_000_000 / max(1, asbd.mSampleRate))
        for chunk in chunks {
            timestampUs &+= usPerChunk
            onChunk?(rate, channelCount, UInt32(chunkFrames), timestampUs, chunk)
        }
    }

    enum CaptureError: Error, LocalizedError {
        case noDisplay

        var errorDescription: String? { "没有可用的显示器用于采集会话" }
    }
}
