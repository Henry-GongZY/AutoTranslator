# AutoTranslator

电脑内部音频实时翻译为字幕的工具。

阶段一目标：**捕获系统内录音频 → Rust core 做 VAD 与识别 → 原生客户端显示字幕悬浮窗**。

## 架构

```
┌──────────────────────────────┐
│ Windows / macOS 原生客户端    │
│ UI、权限、音频捕获、快捷键     │
└───────────┬──────────────────┘
            │ 本地 IPC（命名管道 / Unix socket）
            │ 长度前缀 + Protobuf Envelope
┌───────────▼──────────────────┐
│ translator-core (Rust)        │
│ 混音 → 重采样 → VAD → ASR     │
│ → 字幕稳定 → 回传事件          │
└──────────────────────────────┘
```

- Core 不接触任何平台音频 API，也不含 UI；由客户端启动并监管（类似 Clash Desktop / Clash Core）。
- Core 默认在控制端断开后自动退出（调试可加 `--keep-alive`）。
- 客户端负责 WASAPI 回环捕获（Windows）/ ScreenCaptureKit（macOS，后续）。

## 目录结构

```
proto/translator.proto              唯一的协议真源
crates/translator-protocol/         Rust 侧生成代码 + 帧编解码
crates/translator-core/             core 库与 CLI
  src/ipc/       传输层、请求分发
  src/audio/     PCM 解码、声道混音、窗化 sinc 重采样
  src/vad.rs     自适应噪声底的能量 VAD
  src/asr/       识别器 trait + Mock 实现
  src/subtitle.rs 字幕稳定器（partial/committed、换行、去重）
  src/session.rs 单条流水线编排
clients/windows/Translator.Desktop/ WinUI 3 客户端
```

## 前置条件

- Rust ≥ 1.85（`protoc` 必须在 `PATH` 中，`build.rs` 用它生成 Rust 代码）
  - Windows：`winget install -e --id Google.Protobuf`
- .NET 10 SDK + Windows 10 SDK（本项目用 `10.0.26100.0`）
- Windows App SDK 2.5.1（客户端以 unpackaged + self-contained 方式构建）

## 构建与运行

```powershell
# 1. 构建 core
cargo build --release

# 2. 构建并运行 Windows 客户端
dotnet build clients/windows/Translator.Desktop/Translator.Desktop.csproj -c Release
```

点击「开始监听」后，客户端会：

1. 在输出目录或仓库 `target/` 中查找 `translator-core.exe`（也可用 `TRANSLATOR_CORE` 环境变量指定）；
2. 启动 core 并连接 `\\.\pipe\translator-core-v1`；
3. 握手 → 启动会话 → 开始 WASAPI 回环采集；
4. 把 PCM 帧送给 core，并把返回的 `SubtitleEvent.lines` 渲染到字幕悬浮窗。

## 测试

```powershell
cargo test --workspace
```

包含：重采样 DC 增益与正弦跟踪、VAD 开关、字幕稳定与换行、帧编解码，以及一个
真正的端到端测试——启动 `translator-core.exe` 二进制，用合成语音喂给命名管道，
断言回传了 partial 与 committed 字幕，并验证客户端断开后 core 会退出。

## 协议要点

- 传输：Windows 命名管道 / Unix domain socket，禁止暴露到网络。
- 帧：`[4 字节大端长度][Protobuf Envelope]`，单帧上限 4 MiB。
- 握手：客户端先发 `HandshakeRequest`，协议版本必须一致（当前 `PROTOCOL_VERSION = 1`）。
- 音频：`AudioFrame` 携带裸 PCM，附带采样率/声道/格式与每声道帧数。
- 字幕：`SubtitleEvent.lines` 就是当前应完整显示在屏幕上的文本块，客户端直接渲染即可。

## 阶段一范围

已实现：

- [x] 跨平台 core（IPC、重采样、VAD、字幕稳定、指标）
- [x] Windows 客户端（WASAPI 回环、进程监管、置顶穿透字幕窗）
- [x] 端到端自动化测试

未实现（后续迭代）：

- [ ] 云端流式 ASR（当前为内置 Mock 识别器，无需密钥即可跑通链路）
- [ ] 翻译（`TranslationConfig.provider` 目前只接受 `none`）
- [ ] macOS 客户端（ScreenCaptureKit / Core Audio Process Tap）
- [ ] 按进程捕获音频、字幕导出、自动更新、模型管理

## 说明

- `AudioFrame` 的 PCM 由客户端按设备混音格式发送（通常 48 kHz / 2 声道 / float32），
  core 统一重采样到 16 kHz 单声道后送入 VAD 与识别器，因此平台差异留在客户端。
- 字幕悬浮窗用 `WS_EX_LAYERED` + `LWA_COLORKEY` 把纯黑像素抠成透明，配合
  `WS_EX_TRANSPARENT` 实现点击穿透。

## Windows 多引擎版本

四种 Whisper 引擎、模型目录与自动下载的使用和构建流程见 [Windows 识别说明](docs/windows-recognition.md)。新版完整发布使用 scripts/build-release.ps1。
