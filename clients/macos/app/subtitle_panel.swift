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
        // The user resizes the panel itself; content follows.
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(RoundedRectangle(cornerRadius: 14).fill(Color.black.opacity(0.62)))
    }
}

@MainActor
final class SubtitlePanelController {
    private let panel: NSPanel
    let model = SubtitleModel()

    private static let autosaveName = "SubtitleOverlay"

    init() {
        panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 160),
            styleMask: [.borderless, .nonactivatingPanel, .resizable],
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
        // Drag anywhere on the panel moves it; .resizable gives invisible
        // edge hit zones for resizing; both work without activating the app.
        panel.isMovableByWindowBackground = true
        panel.contentMinSize = NSSize(width: 280, height: 120)
        panel.hidesOnDeactivate = false
        panel.contentView = NSHostingView(rootView: SubtitleOverlayView(model: model))
        // Restores the user's last position and size; the default frame from
        // contentRect is the first-run fallback.
        panel.setFrameAutosaveName(Self.autosaveName)
        if UserDefaults.standard.object(forKey: "NSWindow Frame \(Self.autosaveName)") == nil {
            position()
        }
        setAlwaysOnTop(
            UserDefaults.standard.object(forKey: "OverlayAlwaysOnTop") as? Bool ?? true)
    }

    func setAlwaysOnTop(_ topmost: Bool) {
        panel.level = topmost ? .floating : .normal
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
