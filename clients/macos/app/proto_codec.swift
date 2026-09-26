// Minimal protobuf wire codec for the translator.v1 protocol.
//
// Hand-rolled on purpose: the message set the client exchanges is small and
// stable, and this avoids a SwiftProtobuf/protoc toolchain dependency. Field
// numbers mirror proto/translator.proto and MUST be kept in sync with it (and
// with crates/translator-protocol). Transport: [u32 BE length][Envelope].

import Foundation

// --- wire primitives ---------------------------------------------------------

enum ProtoWire {
    static func varint(_ value: UInt64) -> [UInt8] {
        var v = value
        var out: [UInt8] = []
        while true {
            var byte = UInt8(v & 0x7F)
            v >>= 7
            if v != 0 { byte |= 0x80 }
            out.append(byte)
            if v == 0 { break }
        }
        return out
    }

    static func uint64(_ field: Int, _ value: UInt64) -> [UInt8] {
        guard value != 0 else { return [] } // proto3 omits zero scalars
        return key(field, 0) + varint(value)
    }

    static func bool(_ field: Int, _ value: Bool) -> [UInt8] {
        value ? key(field, 0) + [1] : []
    }

    static func bytes(_ field: Int, _ value: [UInt8]) -> [UInt8] {
        guard !value.isEmpty else { return [] }
        return key(field, 2) + varint(UInt64(value.count)) + value
    }

    static func string(_ field: Int, _ value: String) -> [UInt8] {
        bytes(field, Array(value.utf8))
    }

    /// Submessages are always emitted: presence matters even when empty.
    static func message(_ field: Int, _ value: [UInt8]) -> [UInt8] {
        key(field, 2) + varint(UInt64(value.count)) + value
    }

    static func key(_ field: Int, _ wire: Int) -> [UInt8] {
        varint(UInt64(field << 3 | wire))
    }
}

struct ProtoReader {
    let data: [UInt8]
    var pos = 0

    init(_ data: [UInt8]) { self.data = data }

    var isAtEnd: Bool { pos >= data.count }

    mutating func next() -> Field? {
        guard let k = readVarint() else { return nil }
        let field = Int(k >> 3), wire = Int(k & 7)
        switch wire {
        case 0:
            guard let v = readVarint() else { return nil }
            return Field(field: field, wire: wire, varint: v, bytes: [])
        case 1:
            guard let b = readBytes(8) else { return nil }
            return Field(field: field, wire: wire, varint: 0, bytes: b)
        case 2:
            guard let len = readVarint(), let b = readBytes(Int(len)) else { return nil }
            return Field(field: field, wire: wire, varint: 0, bytes: b)
        case 5:
            guard let b = readBytes(4) else { return nil }
            return Field(field: field, wire: wire, varint: 0, bytes: b)
        default:
            return nil
        }
    }

    struct Field {
        var field: Int
        var wire: Int
        var varint: UInt64
        var bytes: [UInt8]
        var text: String { String(decoding: bytes, as: UTF8.self) }
    }

    private mutating func readVarint() -> UInt64? {
        var result: UInt64 = 0
        var shift: UInt64 = 0
        while true {
            guard pos < data.count else { return nil }
            let byte = data[pos]
            pos += 1
            result |= UInt64(byte & 0x7F) << shift
            if byte & 0x80 == 0 { return result }
            shift += 7
            if shift > 63 { return nil }
        }
    }

    private mutating func readBytes(_ count: Int) -> [UInt8]? {
        guard count >= 0, pos + count <= data.count else { return nil }
        let out = Array(data[pos..<pos + count])
        pos += count
        return out
    }
}

// --- message types (core → client) -------------------------------------------

struct SubtitleInfo {
    var kind = 0 // 1 partial, 2 committed
    var text = ""
    var translated = ""
    var startUs: UInt64 = 0
    var endUs: UInt64 = 0
    var lines: [String] = []
    var isCommitted: Bool { kind == 2 }
}

struct MetricsInfo {
    var audioFrames: UInt64 = 0
    var audioBytes: UInt64 = 0
    var audioMs: UInt64 = 0
    var droppedFrames: UInt64 = 0
    var subtitleEvents: UInt64 = 0
}

enum IncomingMessage {
    case handshake(accepted: Bool, features: [String], error: String)
    case startSession(accepted: Bool, sessionId: String, error: String)
    case setPaused(ok: Bool)
    case stopSession(ok: Bool, error: String)
    case subtitle(SubtitleInfo)
    case status(code: Int, detail: String)
    case error(code: Int, message: String)
    case heartbeat(timestampUs: UInt64)
    case metrics(MetricsInfo)
}

// --- session request config (client → core) -----------------------------------

struct SessionConfig {
    var sessionId: String
    var asrProvider: String // "whisper" (GUI) | "mock" (self-test)
    var inputRate: UInt32
    var inputChannels: UInt32
    var sourceLanguage: String // "" = auto detect (blocks translation)
    var targetLanguage: String
    var translationProvider: String // "none" | "apple-translate"
    var model: String // whisper catalog id: tiny/base/small...
}

// --- envelope ------------------------------------------------------------------

enum Envelope {
    // oneof member field numbers in translator.v1 (members ARE envelope fields)
    private static let handshakeRequest = 2
    private static let startSessionRequest = 4
    private static let stopSessionRequest = 6
    private static let setPausedRequest = 8
    private static let audioFrame = 10
    private static let heartbeatRequest = 15

    static func frame(seq: UInt32, payloadField: Int, payload: [UInt8]) -> [UInt8] {
        var envelope = ProtoWire.uint64(1, UInt64(seq))
        envelope += ProtoWire.message(payloadField, payload)
        let len = envelope.count
        var out = [UInt8((len >> 24) & 0xFF), UInt8((len >> 16) & 0xFF), UInt8((len >> 8) & 0xFF), UInt8(len & 0xFF)]
        out += envelope
        return out
    }

    static func decode(_ frame: [UInt8]) -> IncomingMessage? {
        var reader = ProtoReader(frame)
        while let field = reader.next() {
            if field.field == 1 { continue } // seq
            guard field.wire == 2 else { continue }
            return decodePayload(field.field, field.bytes)
        }
        return nil
    }

    static func decodePayload(_ memberField: Int, _ body: [UInt8]) -> IncomingMessage? {
        switch memberField {
        case 3: // HandshakeResponse
            var accepted = false, error = "", features: [String] = []
            var r = ProtoReader(body)
            while let f = r.next() {
                switch f.field {
                case 1: accepted = f.varint != 0
                case 4: features.append(f.text)
                case 5: error = f.text
                default: break
                }
            }
            return .handshake(accepted: accepted, features: features, error: error)

        case 5: // StartSessionResponse
            var accepted = false, sessionId = "", error = ""
            var r = ProtoReader(body)
            while let f = r.next() {
                switch f.field {
                case 1: accepted = f.varint != 0
                case 2: sessionId = f.text
                case 3: error = f.text
                default: break
                }
            }
            return .startSession(accepted: accepted, sessionId: sessionId, error: error)

        case 7: // StopSessionResponse
            var ok = false, error = ""
            var r = ProtoReader(body)
            while let f = r.next() {
                switch f.field {
                case 1: ok = f.varint != 0
                case 2: error = f.text
                default: break
                }
            }
            return .stopSession(ok: ok, error: error)

        case 9: // SetPausedResponse
            var ok = false
            var r = ProtoReader(body)
            while let f = r.next() {
                if f.field == 1 { ok = f.varint != 0 }
            }
            return .setPaused(ok: ok)

        case 11: // SubtitleEvent
            var info = SubtitleInfo()
            var r = ProtoReader(body)
            while let f = r.next() {
                switch f.field {
                case 3: info.kind = Int(f.varint)
                case 4: info.text = f.text
                case 5: info.translated = f.text
                case 7: info.startUs = f.varint
                case 8: info.endUs = f.varint
                case 10: info.lines.append(f.text)
                default: break
                }
            }
            return .subtitle(info)

        case 12: // StatusEvent
            var code = 0, detail = ""
            var r = ProtoReader(body)
            while let f = r.next() {
                switch f.field {
                case 1: code = Int(f.varint)
                case 2: detail = f.text
                default: break
                }
            }
            return .status(code: code, detail: detail)

        case 13: // MetricsEvent
            var m = MetricsInfo()
            var r = ProtoReader(body)
            while let f = r.next() {
                switch f.field {
                case 1: m.audioFrames = f.varint
                case 2: m.audioBytes = f.varint
                case 3: m.audioMs = f.varint
                case 4: m.droppedFrames = f.varint
                case 6: m.subtitleEvents = f.varint
                default: break
                }
            }
            return .metrics(m)

        case 14: // ErrorEvent
            var code = 0, message = ""
            var r = ProtoReader(body)
            while let f = r.next() {
                switch f.field {
                case 1: code = Int(f.varint)
                case 2: message = f.text
                default: break
                }
            }
            return .error(code: code, message: message)

        case 16: // HeartbeatResponse
            var ts: UInt64 = 0
            var r = ProtoReader(body)
            while let f = r.next() {
                if f.field == 1 { ts = f.varint }
            }
            return .heartbeat(timestampUs: ts)

        default:
            return nil
        }
    }

    // --- outgoing payloads ---

    static func handshake() -> [UInt8] {
        var payload = ProtoWire.uint64(1, 1) // PROTOCOL_VERSION
        payload += ProtoWire.string(2, "mac-client")
        payload += ProtoWire.string(3, "0.1.0")
        return frame(seq: 1, payloadField: handshakeRequest, payload: payload)
    }

    static func startSession(_ cfg: SessionConfig) -> [UInt8] {
        let audioFormat = ProtoWire.uint64(1, UInt64(cfg.inputRate))
            + ProtoWire.uint64(2, UInt64(cfg.inputChannels))
            + ProtoWire.uint64(3, 1) // F32
        let asr = ProtoWire.string(1, cfg.asrProvider)
            + ProtoWire.string(2, cfg.model)
            + ProtoWire.string(3, cfg.sourceLanguage)
        var payload = ProtoWire.string(1, cfg.sessionId)
        payload += ProtoWire.message(2, audioFormat)
        payload += ProtoWire.uint64(3, 16_000) // core down-mixes/resamples to 16 kHz
        payload += ProtoWire.message(4, asr)
        if cfg.translationProvider != "none" {
            let translation = ProtoWire.string(1, cfg.translationProvider)
                + ProtoWire.string(2, cfg.targetLanguage)
            payload += ProtoWire.message(5, translation)
        } else {
            payload += ProtoWire.message(5, ProtoWire.string(1, "none"))
        }
        let subtitle = ProtoWire.uint64(1, 2) + ProtoWire.uint64(2, 42)
            + ProtoWire.uint64(3, 100) + ProtoWire.uint64(4, 300)
        payload += ProtoWire.message(6, subtitle)
        return frame(seq: 2, payloadField: startSessionRequest, payload: payload)
    }

    static func audioFrame(sessionId: String, timestampUs: UInt64, rate: UInt32, channels: UInt32, frames: UInt32, pcm: [UInt8]) -> [UInt8] {
        let format = ProtoWire.uint64(1, UInt64(rate)) + ProtoWire.uint64(2, UInt64(channels)) + ProtoWire.uint64(3, 1) // F32
        var payload = ProtoWire.string(1, sessionId)
        payload += ProtoWire.uint64(2, timestampUs)
        payload += ProtoWire.message(3, format)
        payload += ProtoWire.uint64(4, UInt64(frames))
        payload += ProtoWire.bytes(5, pcm)
        return frame(seq: 3, payloadField: audioFrame, payload: payload)
    }

    static func setPaused(_ paused: Bool) -> [UInt8] {
        frame(seq: 4, payloadField: setPausedRequest, payload: ProtoWire.bool(2, paused))
    }

    static func stopSession(sessionId: String) -> [UInt8] {
        frame(seq: 5, payloadField: stopSessionRequest, payload: ProtoWire.string(1, sessionId))
    }

    static func heartbeat(timestampUs: UInt64) -> [UInt8] {
        frame(seq: 6, payloadField: heartbeatRequest, payload: ProtoWire.uint64(1, timestampUs))
    }
}
