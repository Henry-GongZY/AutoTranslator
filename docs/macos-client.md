# macOS 客户端（apple-platform 分支）

对应 skill 架构图中的"macOS 原生客户端"职责：音频采集、权限、字幕窗口、翻译资产下载交互。

## 构建

```bash
./scripts/build-macos-app.sh
```

产物：

- `target/AutoTranslatorMac.app` — GUI 客户端（内含 Metal 引擎包）
- `target/core-selftest` — 无头自测二进制（协议回环 + mock 会话全链路，先跑它再开 GUI）

## 使用

```bash
open target/AutoTranslatorMac.app
```

1. 首次点「开始」会触发**屏幕录制权限**弹窗（ScreenCaptureKit 采集系统音频需要），在 系统设置 › 隐私与安全性 › 屏幕录制 中允许后重试。
2. 首次会话会按所选模型下载 Whisper 模型（默认 tiny，约 75MB），存放在 app bundle 的 `Resources/models/`。
3. 字幕悬浮窗置顶跟随所有空间；Committed 行下方显示译文字幕。
4. 翻译语言对资产未安装时：控制窗口显示状态，「安装翻译语言资产」按钮通过 `.translationTask` 触发系统下载确认框（这是 Apple 规定的下载交互，辅助进程弹不出来，见 skill references）。

## 自测

```bash
TRANSLATOR_CORE_BIN=engines/metal/translator-core ./target/core-selftest
```

覆盖：protobuf 编解码回环、core 子进程拉起、握手（断言 Metal 引擎特征）、mock ASR + mock 翻译的完整会话（partial 流 → 两条带译文的 Committed → 停止）。GUI 的采集路径（SCStream）无法无头验证，需要人工跑。

## 实现要点

- `proto_codec.swift` — 手写 protobuf wire codec（避免 SwiftProtobuf/protoc 依赖）。**字段号必须与 proto/translator.proto 保持同步**。oneof 成员就是 Envelope 的顶层字段（handshake_response = 字段 3），没有额外的包装层——这是调试中最贵的教训。
- `core_link.swift` — 子进程拉起 + Unix socket 帧协议；读循环独立队列，写串行化；core 断开（exit-on-disconnect）→ 客户端事件。
- `capture.swift` — ScreenCaptureKit 纯音频采集：48 kHz 立体声 Float32，AudioBufferList 直读、重交错、50 ms 分片；音频积压有界（超限丢弃并计数），匹配 core 的有界队列设计。Process Tap 对比留给 skill 路线图的实测项。
- `subtitle_panel.swift` — 非激活 NSPanel 悬浮窗（.floating、joinAllSpaces）。
- `app.swift` — 会话编排 + 控制窗口 + 翻译资产下载交互（真实 App 窗口内的 translationTask，下载确认框可正常弹出）。

## 调试备忘（2026-09-27 实战踩坑）

- **AudioBufferList 尺寸**：立体声非交错需要 `AudioBufferList + 1 个 AudioBuffer` 的空间，只按 `MemoryLayout<AudioBufferList>.size` 分配会让样本提取静默全灭（回调在飞、样本为零）——正确做法是先用 `bufferListSizeNeededOut` 探测所需大小。
- **SCK 两种布局**：非交错（多 AudioBuffer）/ 交错（单 AudioBuffer 多通道）都要兼容，帧数从 `mDataByteSize / 4` 换算。
- **TCC 与签名**：ad-hoc 签名的 App 每次重编译授权就失效；构建脚本用用户的 Apple Development 证书签名（`codesign --force --deep`），授权跨编译保持。TCC 记录重置：`tccutil reset ScreenCapture com.autotranslator.macos`。
- **CGRequestScreenCaptureAccess 在 macOS 15 可能不弹窗**；权限状态用 SCStream.startCapture 的系统弹窗兜底最可靠。
- **诊断日志**：App 事件 → `/tmp/autotranslator-mac.log`；core stderr → `/tmp/translator-core-mac.log`（注意 FileHandle 不创建文件，需先 createFile）。
- 测试配置覆盖：环境变量 `AUTOTRANSLATOR_PROVIDER/SOURCE/TARGET/MODEL`（`launchctl setenv` 可穿透 `open` 启动）。

## 部署与兼容性

跨系统版本兼容、一次构建到处运行、新系统高效替换方案：见 [deployment.md](deployment.md)。

## 已知边界

- 悬浮窗自动位置固定在主屏右下角，未做拖拽记忆。
- GUI 未签名/公证；本地开发构建直接运行即可。
- whisper 首次会话下载模型没有进度条（core 日志里可见）。
