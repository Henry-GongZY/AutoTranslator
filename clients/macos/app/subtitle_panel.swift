// Floating subtitle overlay: a non-activating panel that joins all spaces so
// captions stay visible over video players and full-screen apps.

import AppKit
import SwiftUI

struct SubtitleRow: Identifiable, Equatable {
    let id = UUID()
    var text: String
    var translated: String
    var isPartial: Bool
}

@MainActor
final class SubtitleModel: ObservableObject {
    @Published var rows: [SubtitleRow] = []

    func apply(_ info: SubtitleInfo) {
        var rows = self.rows
        rows.removeAll { $0.isPartial }
        if info.isCommitted {
            // Streaming engines repeat; the stabilizer already dedupes, this
            // is just belt and braces for the visible stack.
            if rows.last?.text != info.text {
                rows.append(SubtitleRow(text: info.text, translated: info.translated, isPartial: false))
            }
        } else if !info.text.isEmpty {
            rows.append(SubtitleRow(text: info.text, translated: info.translated, isPartial: true))
        }
        while rows.count > 4 {
            rows.removeFirst()
        }
        self.rows = rows
    }

    func clear() {
        rows = []
    }
}

struct SubtitleOverlayView: View {
    @ObservedObject var model: SubtitleModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(model.rows) { row in
                VStack(alignment: .leading, spacing: 2) {
                    Text(row.text)
                        .font(.system(size: 17, weight: .medium))
                        .foregroundColor(row.isPartial ? .white.opacity(0.65) : .white)
                        .fixedSize(horizontal: false, vertical: true)
                    if !row.translated.isEmpty {
                        Text(row.translated)
                            .font(.system(size: 15))
                            .foregroundColor(.yellow.opacity(0.95))
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
            if model.rows.isEmpty {
                Text("等待语音…")
                    .font(.system(size: 15))
                    .foregroundColor(.white.opacity(0.4))
            }
        }
        .padding(14)
        .frame(width: 460, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 14).fill(Color.black.opacity(0.62)))
    }
}

@MainActor
final class SubtitlePanelController {
    private let panel: NSPanel
    let model = SubtitleModel()

    init() {
        panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 160),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        panel.isFloatingPanel = true
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .ignoresCycle]
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.isMovable = false
        panel.hidesOnDeactivate = false
        panel.contentView = NSHostingView(rootView: SubtitleOverlayView(model: model))
        position()
    }

    func show() {
        panel.orderFrontRegardless()
    }

    func hide() {
        panel.orderOut(nil)
    }

    func clear() {
        model.clear()
    }

    private func position() {
        guard let screen = NSScreen.main?.visibleFrame else { return }
        let width: CGFloat = 480
        panel.setFrameOrigin(NSPoint(x: screen.maxX - width - 24, y: screen.minY + 96))
    }
}
