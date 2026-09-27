// Floating subtitle overlay: a non-activating panel that joins all spaces so
// captions stay visible over video players and full-screen apps.
//
// Window chrome: titled style with a fully transparent, hidden title bar and
// fullSizeContentView — the caption card paints the whole window (traffic
// lights included), native drag/resize/miniaturize all work, and everything
// is in points so display scaling (HiDPI) needs no special handling.
//
// Content: fixed-size caption strip. New lines push history up (auto-follow
// while pinned to the bottom); the mouse wheel scrolls back through history.

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
    // Scroll state lives on the model: @State pulls in SwiftUI macros, which
    // the bare swiftc build environment cannot always resolve.
    @Published var scrollPosition = ScrollPosition(edge: .bottom)
    @Published var pinnedToBottom = true

    /// History bound: old lines age out of the scrollback.
    private static let maxRows = 300

    func apply(_ info: SubtitleInfo) {
        var rows = self.rows
        rows.removeAll { $0.isPartial }
        if info.isCommitted {
            if rows.last?.text != info.text {
                rows.append(SubtitleRow(text: info.text, translated: info.translated, isPartial: false))
            }
        } else if !info.text.isEmpty {
            rows.append(SubtitleRow(text: info.text, translated: info.translated, isPartial: true))
        }
        while rows.count > Self.maxRows {
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
        ScrollView {
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
                    Text("等待语音…（滚轮回看历史，拖动窗口调整位置）")
                        .font(.system(size: 13))
                        .foregroundColor(.white.opacity(0.4))
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.top, 30) // traffic-light buttons live in this zone
            .padding(.horizontal, 16)
            .padding(.bottom, 12)
        }
        .background(Color.black.opacity(0.62))
        .scrollPosition($model.scrollPosition)
        .onScrollGeometryChange(for: Bool.self) { geometry in
            // At the bottom when the visible bottom edge reaches the content end.
            geometry.contentOffset.y + geometry.containerSize.height
                >= geometry.contentSize.height - 12
        } action: { _, atBottom in
            model.pinnedToBottom = atBottom
        }
        .onChange(of: model.rows.count) { _, _ in
            if model.pinnedToBottom {
                model.scrollPosition.scrollTo(edge: .bottom)
            }
        }
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
            contentRect: NSRect(x: 0, y: 0, width: 460, height: 170),
            styleMask: [
                .titled, .closable, .miniaturizable, .resizable,
                .nonactivatingPanel, .fullSizeContentView,
            ],
            backing: .buffered,
            defer: false
        )
        panel.title = "AutoTranslator 字幕"
        // Transparent hidden titlebar: native chrome (drag zone, resize edges,
        // traffic lights) without a visible bar; content paints beneath it.
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
        panel.contentMinSize = NSSize(width: 300, height: 120)
        panel.hidesOnDeactivate = false
        panel.contentView = OverlayHostingView(rootView: SubtitleOverlayView(model: model))
        // Restores the user's last position and size; the first-run default
        // adapts to the display's point size (HiDPI-safe: all in points).
        panel.setFrameAutosaveName(Self.autosaveName)
        if UserDefaults.standard.object(forKey: "NSWindow Frame \(Self.autosaveName)") == nil {
            position()
        }
        clampFrameToScreen()
        setAlwaysOnTop(
            UserDefaults.standard.object(forKey: "OverlayAlwaysOnTop") as? Bool ?? true)
    }

    /// The window must sit fully inside a display's work area: a frame saved
    /// under a different display arrangement (or dragged halfway off) gets
    /// pulled back on launch.
    private func clampFrameToScreen() {
        let frame = panel.frame
        let center = NSPoint(x: frame.midX, y: frame.midY)
        let screen = NSScreen.screens.first { $0.frame.contains(center) } ?? NSScreen.main
        guard let visible = screen?.visibleFrame else { return }
        var size = frame.size
        size.width = max(300, min(size.width, visible.width))
        size.height = max(120, min(size.height, visible.height))
        let origin = NSPoint(
            x: min(max(frame.minX, visible.minX), visible.maxX - size.width),
            y: min(max(frame.minY, visible.minY), visible.maxY - size.height)
        )
        panel.setFrame(NSRect(origin: origin, size: size), display: true)
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

    /// Landscape caption strip, bottom-right of the main display.
    private func position() {
        guard let screen = NSScreen.main?.visibleFrame else { return }
        let width = min(460, max(380, screen.width / 4))
        let height: CGFloat = 170
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
