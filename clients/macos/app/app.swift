// AutoTranslator macOS client.
//
// Owns (per the architecture boundary): audio capture, permissions, the
// subtitle overlay window and translation-asset download interaction. The
// Rust core keeps session/timeline/stabilisation/translation scheduling.

import AppKit
import SwiftUI
import Translation

/// Diagnostics: everything the client does lands in this file — events,
/// capture format, core lifecycle. Core stderr goes to
/// /tmp/translator-core-mac.log (see CoreLink.launch).
func appLog(_ message: String) {
    let text = "[\(Date().timeIntervalSince1970)] \(message)\n"
    if let handle = FileHandle(forWritingAtPath: "/tmp/autotranslator-mac.log") {
        defer { try? handle.close() }
        handle.seekToEndOfFile()
        handle.write(Data(text.utf8))
    } else {
        try? text.write(toFile: "/tmp/autotranslator-mac.log", atomically: true, encoding: .utf8)
    }
}

// --- session orchestration ----------------------------------------------------

@MainActor
final class SessionController: ObservableObject {
    @Published var running = false
    @Published var statusText = "未启动"
    @Published var errorMessage = ""
    @Published var metricsText = ""
    @Published var sourceLanguage = "en"
    @Published var targetLanguage = "zh-Hans"
    @Published var translationProvider = "apple-translate" // "none" | "apple-translate"
    @Published var model = "tiny"
    @Published var showSubtitlePanel = true
    @Published var overlayAlwaysOnTop =
        UserDefaults.standard.object(forKey: "OverlayAlwaysOnTop") as? Bool ?? true
    /// ASR engine: whisper everywhere; apple-speech requires macOS 26 and
    /// fails the session with a clear state when unavailable (core-side gate).
    @Published var asrProvider = "whisper"
    @Published var assetStatusText = "未检测"
    /// Non-nil while the system asset-download flow should run (drives the
    /// translationTask in the control view).
    @Published var installConfig: TranslationSession.Configuration?

    private var link: CoreLink?
    private var bridgeProcess: Process?
    private let capture = SystemAudioCapture()
    let panel = SubtitlePanelController()
    private var heartbeatTimer: Timer?
    private var audioUs: UInt64 = 0

    init() {
        // Test/deployment overrides: AUTOTRANSLATOR_PROVIDER/SOURCE/TARGET/MODEL
        let env = ProcessInfo.processInfo.environment
        if let v = env["AUTOTRANSLATOR_PROVIDER"], !v.isEmpty { translationProvider = v }
        if let v = env["AUTOTRANSLATOR_ASR"], !v.isEmpty { asrProvider = v }
        if let v = env["AUTOTRANSLATOR_SOURCE"], !v.isEmpty { sourceLanguage = v }
        if let v = env["AUTOTRANSLATOR_TARGET"], !v.isEmpty { targetLanguage = v }
        if let v = env["AUTOTRANSLATOR_MODEL"], !v.isEmpty { model = v }
        appLog("[app] config: provider=\(translationProvider) source=\(sourceLanguage) target=\(targetLanguage) model=\(model)")
    }

    let sourceLanguages: [(id: String, label: String)] = [
        ("en", "英语 English"), ("zh", "中文 Chinese"), ("ja", "日语 日本語"), ("ko", "韩语 한국어"),
    ]
    let targetLanguages: [(id: String, label: String)] = [
        ("zh-Hans", "简体中文"), ("zh-Hant", "繁體中文"), ("en", "English"), ("ja", "日本語"), ("ko", "한국어"),
    ]
    let models: [(id: String, label: String)] = [
        ("tiny", "tiny（最快）"), ("base", "base（均衡）"), ("small", "small（更准）"),
    ]

    // MARK: session lifecycle

    func start() {
        errorMessage = ""
        guard !running else { return }
        appLog("[ui] start pressed (source=\(sourceLanguage) target=\(targetLanguage) provider=\(translationProvider) asr=\(asrProvider) model=\(model))")
        if asrProvider == "apple-speech" {
            do { try ensureBridge() } catch {
                errorMessage = error.localizedDescription
                return
            }
        }

        if translationProvider != "none" && sourceLanguage.isEmpty {
            errorMessage = "翻译需要明确的源语言；选“自动检测”时请先把翻译关掉。"
            return
        }
        if asrProvider == "apple-speech" && sourceLanguage.isEmpty {
            errorMessage = "Apple 系统识别不支持自动检测语言，请选择源语言。"
            return
        }
        // No CGPreflight gate here: on macOS 15 CGRequestScreenCaptureAccess
        // can silently refuse to prompt. SCStream.startCapture below triggers
        // the reliable system prompt on its own.
        appLog("[ui] permission preflight=\(SystemAudioCapture.hasPermission())")

        let link = CoreLink()
        let socketPath = "/tmp/translator-core-mac-\(Int.random(in: 1000...9999)).sock"
        do {
            try link.launch(binary: resolveCoreBinary(), socketPath: socketPath) { [weak self] event in
                Task { @MainActor in self?.handle(event) }
            }
        } catch {
            appLog("[ui] core launch failed: \(error)")
            errorMessage = error.localizedDescription
            return
        }
        self.link = link
        statusText = "正在连接 core…"
        link.handshake()
    }

    func stop() {
        guard running else { return }
        statusText = "正在停止…"
        capture.stop()
        link?.stopSession()
    }

    func shutdown() {
        heartbeatTimer?.invalidate()
        capture.stop()
        link?.shutdown()
        link = nil
        bridgeProcess?.terminate()
        bridgeProcess = nil
    }

    // MARK: events from the core link

    private func handle(_ event: CoreLink.LinkEvent) {
        appLog("[link] \(event)")
        switch event {
        case .connected(let features):
            statusText = "已连接 core（\(features.contains("asr.whisper.engine.metal") ? "Metal 引擎" : "CPU 引擎")），正在启动会话…"
            let cfg = SessionConfig(
                sessionId: "mac-\(UUID().uuidString.prefix(8))",
                asrProvider: asrProvider,
                inputRate: 48_000,
                inputChannels: 2,
                sourceLanguage: sourceLanguage,
                targetLanguage: targetLanguage,
                translationProvider: translationProvider,
                model: asrProvider == "whisper" ? model : ""
            )
            link?.startSession(cfg)

        case .sessionStarted(let ok, let error):
            guard ok else {
                var message = error
                if message.contains("not installed") {
                    message += "\n点击下方「安装翻译语言资产」完成一次性下载后重试。"
                }
                errorMessage = message
                statusText = "会话被拒绝"
                teardownLink()
                return
            }
            running = true
            statusText = "监听中"
            panel.clear()
            if showSubtitlePanel { panel.show() }
            startCapture()
            startHeartbeat()

        case .sessionStopped(let ok, let error):
            running = false
            statusText = ok ? "已停止" : "已停止（\(error)）"
            stopHeartbeat()
            teardownLink() // kill the core: every 开始 gets a fresh engine
            if !error.isEmpty && !ok { errorMessage = error }

        case .subtitle(let info):
            panel.model.apply(info)

        case .status(_, let detail):
            if running { statusText = "监听中 · \(detail)" }

        case .error(_, let message):
            if message.contains("no active session") { return } // stale audio after stop
            errorMessage = message

        case .metrics(let m):
            metricsText = "音频 \(m.audioMs)ms · 丢帧 \(m.droppedFrames) · 字幕 \(m.subtitleEvents) 条"

        case .disconnected:
            if running {
                running = false
                statusText = "与 core 断开"
                stopHeartbeat()
                errorMessage = "translator-core 进程断开，请重新开始。"
            }
        }
    }

    private func startCapture() {
        audioUs = 0
        guard let link = self.link else { return }
        capture.onChunk = { rate, channels, frames, timestampUs, pcm in
            // ScreenCaptureKit delivers 48 kHz stereo Float32.
            link.sendAudio(rate: rate, channels: channels, frames: frames, timestampUs: timestampUs, pcm: pcm)
        }
        Task {
            do {
                try await capture.start()
            } catch {
                errorMessage = "音频采集失败：\((error as? LocalizedError)?.errorDescription ?? error.localizedDescription)"
                stop()
            }
        }
    }

    private func startHeartbeat() {
        heartbeatTimer?.invalidate()
        heartbeatTimer = Timer.scheduledTimer(withTimeInterval: 20, repeats: true) { [weak self] _ in
            self?.link?.heartbeat()
        }
    }

    private func stopHeartbeat() {
        heartbeatTimer?.invalidate()
        heartbeatTimer = nil
    }

    private func teardownLink() {
        link?.shutdown()
        link = nil
    }

    // MARK: translation asset download

    func refreshAssetStatus() {
        let source = sourceLanguage
        let target = targetLanguage
        Task {
            let availability = LanguageAvailability()
            let status = await availability.status(
                from: Locale.Language(identifier: source),
                to: Locale.Language(identifier: target)
            )
            assetStatusText = describe(status)
        }
    }

    func installAssets() {
        installConfig = TranslationSession.Configuration(
            source: Locale.Language(identifier: sourceLanguage),
            target: Locale.Language(identifier: targetLanguage)
        )
    }

    private func describe(_ status: LanguageAvailability.Status) -> String {
        switch status {
        case .installed: return "已安装，可离线翻译"
        case .supported: return "支持但资产未安装（需一次性下载）"
        case .unsupported: return "此系统不支持该语言对"
        @unknown default: return "未知状态"
        }
    }

    // MARK: helpers

    /// Spawn the Swift bridge when it is not already running: the apple-speech
    /// and apple-translate providers both talk to it over its socket.
    private func ensureBridge() throws {
        if FileManager.default.fileExists(atPath: "/tmp/translator-bridge-v1.sock") {
            // Assume a live bridge; a stale socket fails fast on connect.
            return
        }
        let binary = ProcessInfo.processInfo.environment["TRANSLATOR_BRIDGE_BIN"]
            ?? Bundle.main.path(forResource: "translator-bridge", ofType: nil)
            ?? "target/translator-bridge"
        guard FileManager.default.fileExists(atPath: binary) else {
            throw CoreLinkError.spawnFailed("translator-bridge 不存在（\(binary)），请运行 scripts/build-bridge.sh")
        }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: binary)
        if let stderr = FileHandle(forWritingAtPath: "/tmp/translator-bridge-mac.log") {
            stderr.seekToEndOfFile()
            process.standardError = stderr
        } else {
            FileManager.default.createFile(atPath: "/tmp/translator-bridge-mac.log", contents: nil)
            if let stderr = FileHandle(forWritingAtPath: "/tmp/translator-bridge-mac.log") {
                process.standardError = stderr
            }
        }
        try process.run()
        bridgeProcess = process
        appLog("[bridge] spawned \(binary)")
        // Give the socket a moment; the provider's connect has its own retry.
        Thread.sleep(forTimeInterval: 0.5)
    }

    private func resolveCoreBinary() -> String {
        if let env = ProcessInfo.processInfo.environment["TRANSLATOR_CORE_BIN"], !env.isEmpty {
            return env
        }
        if let bundled = Bundle.main.path(forResource: "translator-core", ofType: nil) {
            return bundled
        }
        return "engines/metal/translator-core"
    }
}

// --- UI -------------------------------------------------------------------------

struct ControlView: View {
    @ObservedObject var controller: SessionController

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("AutoTranslator · 实时系统音频字幕")
                .font(.headline)

            Picker("源语言", selection: $controller.sourceLanguage) {
                ForEach(controller.sourceLanguages, id: \.id) { item in
                    Text(item.label).tag(item.id)
                }
            }
            Picker("目标语言", selection: $controller.targetLanguage) {
                ForEach(controller.targetLanguages, id: \.id) { item in
                    Text(item.label).tag(item.id)
                }
            }
            Picker("识别引擎", selection: $controller.asrProvider) {
                Text("Whisper（本地 Metal 推理）").tag("whisper")
                Text("Apple 系统识别（需 macOS 26）").tag("apple-speech")
            }
            if controller.asrProvider == "whisper" {
                Picker("识别模型", selection: $controller.model) {
                    ForEach(controller.models, id: \.id) { item in
                        Text(item.label).tag(item.id)
                    }
                }
            }
            Picker("翻译", selection: $controller.translationProvider) {
                Text("Apple 系统翻译（离线）").tag("apple-translate")
                Text("不翻译").tag("none")
            }

            TranslationAssetSection(controller: controller)

            HStack(spacing: 12) {
                if controller.running {
                    Button("停止") { controller.stop() }
                        .keyboardShortcut(.cancelAction)
                } else {
                    Button("开始") { controller.start() }
                        .keyboardShortcut(.defaultAction)
                }
                Toggle("字幕悬浮窗", isOn: $controller.showSubtitlePanel)
                    .toggleStyle(.checkbox)
                Toggle("置顶", isOn: $controller.overlayAlwaysOnTop)
                    .toggleStyle(.checkbox)
            }

            Text(controller.statusText)
                .font(.callout)
                .foregroundColor(.secondary)
            if !controller.metricsText.isEmpty {
                Text(controller.metricsText)
                    .font(.caption)
                    .foregroundColor(.secondary)
            }
            if !controller.errorMessage.isEmpty {
                Text(controller.errorMessage)
                    .font(.callout)
                    .foregroundColor(.red)
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
            }
        }
        .padding(18)
        .frame(width: 420, alignment: .leading)
        .onChange(of: controller.showSubtitlePanel) { _, shown in
            if shown { controller.panel.show() } else { controller.panel.hide() }
        }
        .onChange(of: controller.overlayAlwaysOnTop) { _, topmost in
            controller.panel.setAlwaysOnTop(topmost)
            UserDefaults.standard.set(topmost, forKey: "OverlayAlwaysOnTop")
        }
        .onAppear { controller.refreshAssetStatus() }
        // The system language-asset download prompt attaches here, in a real
        // visible window — exactly the interaction a helper process cannot do.
        .translationTask(controller.installConfig) { session in
            do {
                try await session.prepareTranslation()
                controller.installConfig = nil
                controller.refreshAssetStatus()
            } catch {
                controller.installConfig = nil
                controller.errorMessage = "翻译资产下载失败：\(error.localizedDescription)"
            }
        }
    }
}

struct TranslationAssetSection: View {
    @ObservedObject var controller: SessionController

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text("翻译资产").font(.subheadline).foregroundColor(.secondary)
                Text(controller.assetStatusText).font(.caption).foregroundColor(.orange)
                Spacer()
                Button("检测") { controller.refreshAssetStatus() }
                    .controlSize(.small)
                if controller.assetStatusText.contains("未安装") {
                    Button("安装翻译语言资产") { controller.installAssets() }
                        .controlSize(.small)
                }
            }
        }
    }
}

// --- entry -----------------------------------------------------------------------

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let controller = SessionController()

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false // keep running with just the subtitle overlay
    }

    func applicationWillTerminate(_ notification: Notification) {
        controller.shutdown()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        appLog("[app] launched")
        if CommandLine.arguments.contains("--auto-start") {
            // Retry until the screen-recording permission is granted, so the
            // session starts by itself right after the user allows it.
            var retries = 0
            Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { [weak self] timer in
                retries += 1
                guard retries < 24 else { timer.invalidate(); return }
                Task { @MainActor in
                    guard let self, !self.controller.running else { timer.invalidate(); return }
                    self.controller.start()
                }
            }
        }
    }
}

@main
struct AutoTranslatorApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        WindowGroup("AutoTranslator") {
            ControlView(controller: delegate.controller)
        }
    }
}
