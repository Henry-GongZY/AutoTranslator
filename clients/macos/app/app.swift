// AutoTranslator macOS client.
//
// Owns (per the architecture boundary): audio capture, permissions, the
// subtitle overlay window and translation-asset download interaction. The
// Rust core keeps session/timeline/stabilisation/translation scheduling.

import AppKit
import ScreenCaptureKit
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

/// UI language follows the system's preferred language: Chinese for zh-*,
/// English otherwise.
private let prefersChinese = Locale.preferredLanguages.first?.hasPrefix("zh") ?? true

/// Bilingual string lookup for the control UI.
func L(_ zh: String, _ en: String) -> String {
    prefersChinese ? zh : en
}

// --- session orchestration ----------------------------------------------------

@MainActor
final class SessionController: ObservableObject {
    @Published var running = false
    @Published var statusText = L("未启动", "Not started")
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
    /// Overlay card material (Liquid Glass needs macOS 26; ignored below).
    @Published var overlayGlass = UserDefaults.standard.object(forKey: "OverlayUseGlass") as? Bool ?? true
    // Cloud translation credentials — persisted on change.
    @Published var translationApiKey: String = UserDefaults.standard.string(forKey: "TranslationApiKey") ?? "" {
        didSet { UserDefaults.standard.set(translationApiKey, forKey: "TranslationApiKey") }
    }
    @Published var translationApiBase: String = UserDefaults.standard.string(forKey: "TranslationApiBase") ?? "" {
        didSet { UserDefaults.standard.set(translationApiBase, forKey: "TranslationApiBase") }
    }
    @Published var translationModel: String = UserDefaults.standard.string(forKey: "TranslationModel") ?? "" {
        didSet { UserDefaults.standard.set(translationModel, forKey: "TranslationModel") }
    }
    @Published var translationAppId: String = UserDefaults.standard.string(forKey: "TranslationAppId") ?? "" {
        didSet { UserDefaults.standard.set(translationAppId, forKey: "TranslationAppId") }
    }

    var translationUsesCloud: Bool {
        ["openai", "deepl", "google", "baidu"].contains(translationProvider)
    }
    var translationNeedsBase: Bool { translationProvider == "openai" }
    var translationNeedsModel: Bool { translationProvider == "openai" }
    var translationNeedsAppId: Bool { translationProvider == "baidu" }
    @Published var assetStatusText = L("未检测", "Not checked")
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
        ("tiny", L("tiny（最快）", "tiny (fastest)")),
        ("base", L("base（均衡）", "base (balanced)")),
        ("small", L("small（更准）", "small (more accurate)")),
    ]

    // MARK: session lifecycle

    func start() {
        errorMessage = ""
        guard !running else { return }
        appLog("[ui] start pressed (source=\(sourceLanguage) target=\(targetLanguage) provider=\(translationProvider) asr=\(asrProvider) model=\(model))")
        // apple-speech 识别和 apple-translate 翻译都走同一个桥接进程。
        if asrProvider == "apple-speech" || translationProvider == "apple-translate" {
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
        if translationUsesCloud {
            if translationApiKey.trimmingCharacters(in: .whitespaces).isEmpty {
                errorMessage = "所选翻译服务需要填写 API Key。"
                return
            }
            if translationProvider == "baidu" && translationAppId.trimmingCharacters(in: .whitespaces).isEmpty {
                errorMessage = "百度翻译需要填写 APP ID 和密钥。"
                return
            }
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
        statusText = L("正在连接 core…", "Connecting to core…")
        link.handshake()
    }

    /// Wipe the caption overlay's visible history (new captions keep flowing).
    func clearCaptions() {
        panel.clear()
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
            statusText = L("已连接 core（Metal 引擎），正在启动会话…", "Core connected (Metal engine), starting session…")
            let cfg = SessionConfig(
                sessionId: "mac-\(UUID().uuidString.prefix(8))",
                asrProvider: asrProvider,
                inputRate: 48_000,
                inputChannels: 2,
                sourceLanguage: sourceLanguage,
                targetLanguage: targetLanguage,
                translationProvider: translationProvider,
                model: asrProvider == "whisper" ? model : "",
                translationApiKey: translationApiKey,
                translationApiBase: translationApiBase,
                translationModel: translationModel,
                translationAppId: translationAppId
            )
            link?.startSession(cfg)

        case .sessionStarted(let ok, let error):
            guard ok else {
                var message = error
                if message.contains("not installed") {
                    message += "\n" + L("点击下方「安装翻译语言资产」完成一次性下载后重试。",
                        "Click \"Install translation language assets\" below, complete the one-time download and start again.")
                }
                errorMessage = message
                statusText = L("会话被拒绝", "Session rejected")
                teardownLink()
                return
            }
            running = true
            statusText = L("监听中", "Listening")
            panel.clear()
            if showSubtitlePanel { panel.show() }
            startCapture()
            startHeartbeat()

        case .sessionStopped(let ok, let error):
            running = false
            statusText = ok ? L("已停止", "Stopped") : L("已停止（\(error)）", "Stopped (\(error))")
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
            metricsText = L("音频 \(m.audioMs)ms · 丢帧 \(m.droppedFrames) · 字幕 \(m.subtitleEvents) 条",
                            "audio \(m.audioMs)ms · dropped \(m.droppedFrames) · captions \(m.subtitleEvents)")

        case .disconnected:
            if running {
                running = false
                statusText = L("与 core 断开", "Disconnected from core")
                stopHeartbeat()
                errorMessage = L("translator-core 进程断开，请重新开始。", "translator-core disconnected — start again.")
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
                errorMessage = L("音频采集失败：", "Audio capture failed: ")
                    + ((error as? LocalizedError)?.errorDescription ?? error.localizedDescription)
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
        case .installed: return L("已安装，可离线翻译", "Installed — offline translation ready")
        case .supported: return L("支持但资产未安装（需一次性下载）", "Supported — assets not installed (one-time download)")
        case .unsupported: return L("此系统不支持该语言对", "Pair unsupported on this system")
        @unknown default: return L("未知状态", "Unknown")
        }
    }

    // MARK: helpers

    /// Spawn the Swift bridge when it is not actually reachable: the apple-speech
    /// and apple-translate providers both talk to it over its socket. A socket
    /// FILE alone proves nothing — a killed bridge leaves one behind and the
    /// core would get ECONNREFUSED — so probe with a real connection.
    private func ensureBridge() throws {
        let socketPath = "/tmp/translator-bridge-v1.sock"
        if bridgeSocketAlive(socketPath) {
            return
        }
        appLog("[bridge] socket not answering, respawning")
        // A stale socket file blocks the new bind; the bridge also removes
        // one at bind, but clear it here so the probe below is honest.
        try? FileManager.default.removeItem(atPath: socketPath)
        bridgeProcess?.terminate()
        bridgeProcess = nil

        let binary = ProcessInfo.processInfo.environment["TRANSLATOR_BRIDGE_BIN"]
            ?? Bundle.main.path(forResource: "translator-bridge", ofType: nil)
            ?? "target/translator-bridge"
        guard FileManager.default.fileExists(atPath: binary) else {
            throw CoreLinkError.spawnFailed("translator-bridge 不存在（\(binary)），请运行 scripts/build-bridge.sh")
        }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: binary)
        FileManager.default.createFile(atPath: "/tmp/translator-bridge-mac.log", contents: nil)
        if let stderr = FileHandle(forWritingAtPath: "/tmp/translator-bridge-mac.log") {
            stderr.seekToEndOfFile()
            process.standardError = stderr
        }
        try process.run()
        bridgeProcess = process
        appLog("[bridge] spawned \(binary)")

        // Wait for the socket to actually answer (up to ~5 s).
        for _ in 0..<50 {
            if bridgeSocketAlive(socketPath) {
                appLog("[bridge] alive")
                return
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        throw CoreLinkError.spawnFailed("translator-bridge 已启动但未监听，详见 /tmp/translator-bridge-mac.log")
    }

    /// True when something is listening on the Unix socket.
    private func bridgeSocketAlive(_ path: String) -> Bool {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return false }
        defer { close(fd) }
        let bytes = Array(path.utf8)
        guard bytes.count < 104 else { return false }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &addr.sun_path) { dest in
            dest.copyBytes(from: bytes)
        }
        let result = withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                connect(fd, sa, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        return result == 0
    }

    /// Debug: screenshot our own overlay window (the app holds the Screen
    /// Recording permission, so this works headlessly) for UI debugging.
    func captureOverlayToTmp() {
        panel.show()
        Task {
            do {
                let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
                let number = self.panel.windowNumber
                guard let scWindow = content.windows.first(where: { $0.windowID == CGWindowID(number) }) else {
                    appLog("[capture-overlay] overlay window not found in shareable content")
                    return
                }
                let filter = SCContentFilter(desktopIndependentWindow: scWindow)
                let config = SCStreamConfiguration()
                config.width = Int(self.panel.pixelSize.width * 2)
                config.height = Int(self.panel.pixelSize.height * 2)
                config.showsCursor = false
                let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
                let rep = NSBitmapImageRep(cgImage: image)
                guard let data = rep.representation(using: .png, properties: [:]) else { return }
                try data.write(to: URL(fileURLWithPath: "/tmp/overlay.png"))
                appLog("[capture-overlay] saved /tmp/overlay.png")
                exit(0)
            } catch {
                appLog("[capture-overlay] failed: \(error)")
                exit(1)
            }
        }
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

            Picker(L("源语言", "Source"), selection: $controller.sourceLanguage) {
                ForEach(controller.sourceLanguages, id: \.id) { item in
                    Text(item.label).tag(item.id)
                }
            }
            Picker(L("目标语言", "Target"), selection: $controller.targetLanguage) {
                ForEach(controller.targetLanguages, id: \.id) { item in
                    Text(item.label).tag(item.id)
                }
            }
            Picker(L("识别引擎", "Recognition engine"), selection: $controller.asrProvider) {
                Text(L("Whisper（本地 Metal 推理）", "Whisper (local Metal inference)")).tag("whisper")
                Text(L("Apple 系统识别（需 macOS 26）", "Apple Speech (macOS 26+)")).tag("apple-speech")
            }
            if controller.asrProvider == "whisper" {
                Picker(L("识别模型", "Model"), selection: $controller.model) {
                    ForEach(controller.models, id: \.id) { item in
                        Text(item.label).tag(item.id)
                    }
                }
            }
            if #available(macOS 26.0, *) {
                Picker(L("悬浮窗材质", "Overlay material"), selection: $controller.overlayGlass) {
                    Text("Liquid Glass").tag(true)
                    Text(L("纯色半透明", "Translucent black")).tag(false)
                }
            }
            Picker(L("翻译", "Translation"), selection: $controller.translationProvider) {
                Text(L("Apple 系统翻译（离线，15+）", "Apple Translation (on-device, 15+)")).tag("apple-translate")
                Text(L("OpenAI 兼容 API", "OpenAI-compatible API")).tag("openai")
                Text("DeepL API").tag("deepl")
                Text(L("Google 翻译 API", "Google Translate API")).tag("google")
                Text(L("百度翻译 API", "Baidu Translate API")).tag("baidu")
                Text(L("不翻译", "No translation")).tag("none")
            }
            if controller.translationUsesCloud {
                Group {
                    if controller.translationNeedsBase {
                        TextField(L("API 地址（留空 = 官方端点）", "API base URL (blank = official)"), text: $controller.translationApiBase)
                    }
                    if controller.translationNeedsModel {
                        TextField(L("模型名（如 gpt-4o-mini / deepseek-chat）", "Model (e.g. gpt-4o-mini / deepseek-chat)"), text: $controller.translationModel)
                    }
                    if controller.translationNeedsAppId {
                        TextField(L("百度 APP ID", "Baidu APP ID"), text: $controller.translationAppId)
                    }
                    SecureField("API Key", text: $controller.translationApiKey)
                }
            }

            TranslationAssetSection(controller: controller)

            HStack(spacing: 12) {
                if controller.running {
                    Button(L("停止", "Stop")) { controller.stop() }
                        .keyboardShortcut(.cancelAction)
                } else {
                    Button(L("开始", "Start")) { controller.start() }
                        .keyboardShortcut(.defaultAction)
                }
                Button(L("清空字幕", "Clear captions")) { controller.clearCaptions() }
                Toggle(L("字幕悬浮窗", "Subtitle overlay"), isOn: $controller.showSubtitlePanel)
                    .toggleStyle(.checkbox)
                Toggle(L("置顶", "Keep on top"), isOn: $controller.overlayAlwaysOnTop)
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
        .onChange(of: controller.overlayGlass) { _, glass in
            controller.panel.model.useGlass = glass
            UserDefaults.standard.set(glass, forKey: "OverlayUseGlass")
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
                Text(L("翻译资产", "Translation assets")).font(.subheadline).foregroundColor(.secondary)
                Text(controller.assetStatusText).font(.caption).foregroundColor(.orange)
                Spacer()
                Button(L("检测", "Check")) { controller.refreshAssetStatus() }
                    .controlSize(.small)
                if controller.assetStatusText.contains("未安装") {
                    Button(L("安装翻译语言资产", "Install translation language assets")) { controller.installAssets() }
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
        if CommandLine.arguments.contains("--capture-overlay") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in
                self?.controller.captureOverlayToTmp()
            }
        }
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
