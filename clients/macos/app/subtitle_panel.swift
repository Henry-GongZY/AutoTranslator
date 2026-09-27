// Floating subtitle overlay: a non-activating panel that joins all spaces so
// captions stay visible over video players and full-screen apps.
//
// Window chrome: titled style with a fully transparent, hidden title bar —
// the user gets native drag, native edge-resize and working close/miniaturize
// buttons, while the caption card keeps its borderless look. Everything is in
// points, so display scaling (HiDPI) needs no special handling.

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
                Text("等待语音…（拖动窗口调整位置，拖边缘改变大小）")
                    .font(.system(size: 13))
                    .foregroundColor(.white.opacity(0.4))
            }
        }
        .padding(.top, 30) // traffic-light buttons live in this zone
        .padding(.horizontal, 16)
        .padding(.bottom, 14)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color.black.opacity(0.62))
    }
}

/// Hosting view that lets `isMovableByWindowBackground` work through SwiftUI
/// content (the default hosting view reports a non-movable background).
final class OverlayHostingView: NSHostingView<SubtitleOverlayView> {
    override var mouseDownCanMoveWindow: Bool { true }
}

@MainActor
final class SubtitlePanelController {
    private let panel: NSPanel
    let model = SubtitleModel()

    private static let autosaveName = "SubtitleOverlay"

    init() {
        panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 200),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        panel.title = "AutoTranslator 字幕"
        // Transparent hidden titlebar: native chrome (drag zone, resize edges,
        // traffic lights) without a visible bar.
        panel.titlebarAppearsTransparent = true
        panel.titleVisibility = .hidden
        panel.standardWindowButton(.zoomButton)?.isHidden = true
        panel.isFloatingPanel = true
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .ignoresCycle]
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.isMovable = true
        panel.isMovableByWindowBackground = true
        panel.contentMinSize = NSSize(width: 300, height: 140)
        panel.hidesOnDeactivate = false
        panel.contentView = OverlayHostingView(rootView: SubtitleOverlayView(model: model))
        // Restores the user's last position and size; the first-run default
        // adapts to the display's point size (HiDPI-safe: all in points).
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
        let width = min(560, max(380, screen.width / 4))
        let height: CGFloat = 200
        panel.setFrame(
            NSRect(
                x: screen.maxX - width - 24,
                y: screen.minY + 96,
                width: width,
                height: height
            ),
            display: true
        )
    }
}
