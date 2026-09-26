---
name: apple-platform-dev
description: AutoTranslator 的 Apple 平台开发指南（apple-platform 分支）。已固化的架构决策：保留跨平台 Rust core，新增 Apple 专用构建与平台后端，接入 whisper.cpp Metal、Apple Translation、SpeechAnalyzer。凡在本仓库做 macOS / Apple Silicon 相关开发——提到 Mac、macOS、Metal、Core ML、ANE、SpeechAnalyzer、SpeechTranscriber、Translation 框架、Process Tap、ScreenCaptureKit、Swift 桥接，或想"在 Mac 上跑 / 接系统识别 / 接系统翻译"——都必须先读本 skill，即使用户没有点名 Apple。
---

# Apple 平台开发指南（apple-platform 分支）

## 方向（已定，不要重开讨论）

**一个 Rust core，多种平台后端，Apple 专用构建。**

- Rust core 继续负责：会话、时间轴、字幕稳定、翻译调度、缓存、指标。它是跨平台的，不为 Apple 用 Swift 重写——把 core 整体改成 Swift 并不会自然提高推理速度。
- Apple 平台专属部分：构建 feature（Metal / Core ML）、macOS 客户端（音频采集、权限、字幕窗口、模型下载交互）、Swift 桥接（SpeechAnalyzer、Translation）。
- Swift 桥接层可以放在客户端或独立辅助进程，Rust 通过本地 IPC 调用；模型推理不需要因为这层边界而迁移整套业务。Rust 负责通用业务，Apple SDK 负责适合它的硬件加速与系统能力，二者组合而非替换。

```
macOS 原生客户端
  音频采集、权限、字幕窗口、模型下载交互
           │
     Rust 通用核心
  会话、时间轴、字幕稳定、翻译调度、指标
           │
   可替换的平台后端
  ├─ Whisper Metal / Core ML
  ├─ Swift 桥接：SpeechAnalyzer
  └─ Swift 桥接：Translation
```

## 路线图（按此顺序推进）

1. **Apple Silicon 原生构建：Rust + Whisper Metal。** 复用现有代码与模型；接通运行时设备选择和能力报告；同时处理推理阻塞 IPC（见"性能守则"）。最先落地，因为改动最集中。
2. **接 Apple Translation。** 打通"音频 → 原文 → 译文字幕"完整链路，作为第一版 macOS 文本翻译后端。两个后端（识别、翻译）的选择互相独立，不必绑定。
3. **macOS 26 上并行评估 SpeechAnalyzer 后端。** 若目标语种准确率达标、延迟与功耗更好，让它成为该配置下的默认选择。
4. **再比较 Core ML / WhisperKit。** 按实测收益决定是否承担额外的模型资产与打包成本；没有明确需求不引入。

第一版落点 = **Whisper Metal + Apple Translation**；**SpeechAnalyzer + Apple Translation** 是 macOS 26 上争取更低延迟/功耗的候选。M4 及后续芯片沿用同一套能力检测 + 实测选择机制，不为每代 SoC 分叉维护整个 core。

## 现状缺口（动手前先核对这些锚点）

以下位置已对照源码核实（2026-09，main 分支）：

- `crates/translator-core/Cargo.toml:8-18` — features 只暴露 `cuda` / `blas` / `vulkan`。锁定的 whisper-rs 0.14.4 已支持 `metal`、`coreml`，尚未接通。Apple CPU 路径可走 Accelerate（BLAS），但这不等于 GPU/ANE 加速。
- `crates/translator-core/src/asr/whisper.rs:444` — `use_gpu` 只在 cuda/vulkan feature 下为 true。只在编译参数打开 Metal feature 不完整，运行时设备选择要一起改。
- `crates/translator-core/src/asr/whisper.rs:31-38` — 每 2 秒（`STEP_SECONDS`）重解码最长 12 秒（`WINDOW_SECONDS`）窗口。硬件加速能缩短单次计算，但消除不了重复解码与等待下一轮的延迟——这是实时调度问题，不是换个 SDK 就消失的问题。
- `crates/translator-core/src/session.rs:89-97` — phase 1 拒绝 `none` 以外的翻译 provider。语音识别和文本翻译要分别选实现。
- `crates/translator-core/src/asr/mod.rs:59-78` — `SpeechRecognizer` 是 `push_audio → Vec<Event>` 的拉取式接口，固定 16 kHz。Apple 原生后端需要流式事件与格式协商（见下节）。
- `crates/translator-core/src/ipc/mod.rs:54-58` — 处理循环串行：`dispatch`（含同步 Whisper 推理）完成前，同一连接上后续的音频与控制消息都在等待。

仓库目前只有 `clients/windows`；macOS 客户端在本分支新建。

## 接口演进的四条硬要求

新增 Apple 后端不能只加一个 provider 名称：

1. **流式识别接口。** 长期支持"持续送入音频 + 独立接收识别事件"——SpeechAnalyzer 的输入流与结果流本就彼此独立。演进 `SpeechRecognizer` 时保持 Whisper 后端兼容，或引入并行的 streaming trait。
2. **格式协商。** 当前统一 16 kHz 适合 Whisper；Apple 后端应按 `bestAvailableAudioFormat` 协商输入格式，避免不必要的重复重采样。采样率/声道/格式应是会话协商的结果，不是常量。
3. **provider 与 device 分离。** `whisper`、`apple-speech` 是 provider（模型/识别服务）；`cpu`、`metal`、`coreml` 是执行方式（计算设备）。Core ML 可能与其他计算路径组合，不要把它塞进 provider 枚举。
4. **后端状态上报。** 原生后端要能表达：资产未安装、语言不支持、系统不可用（macOS 版本不够）等状态，客户端据此明确选择回退（如 SpeechAnalyzer 不可用 → Whisper Metal），不能靠猜。

## 术语红线（防最常见的混淆）

- **Metal** = GPU。现有 whisper.cpp 路线最容易接上。
- **Core ML** = 系统在 CPU/GPU/ANE 之间选择受支持的执行方式。启用它不代表全部计算进 ANE，也不保证比 Metal 快。whisper.cpp 的 Core ML 路径主要加速 encoder；额外模型加载、首次编译、decoder 耗时都必须计入端到端结果。
- **MLX** = Apple 开源 ML 框架，当前主要面向 CPU/GPU 与统一内存。用 MLX ≠ 用 ANE。
- whisper.cpp 文档中"相对 CPU 的加速数字"不能当作"相对 Metal 的收益"。
- WhisperKit 是第三方方案（非 Apple 官方 SDK），仅在需要自带可固定版本模型、深入优化 Core ML 时评估，不是当前项目的必经层。

## 性能守则（与换 SDK 同等重要）

IPC 处理循环的推理阻塞优先解决：

- 音频接收、推理、字幕输出三者分离，各自设**有界队列**。
- 对过期的 partial 推理请求做**合并**，避免越积越慢。
- 模型常驻内存，测量并优化冷启动与预热。
- 翻译只对**稳定文本**片段做；频繁变化的 partial 去抖、取消、缓存。
- 控制提交粒度：如果只等整句结束才翻译，长句会产生明显延迟。

本地 IPC 暂不替换为共享内存：48 kHz 双声道 float32 约 384 KB/s，量级不大。先测队列等待、推理与提交策略，再决定是否需要共享内存。

## macOS 音频采集（放在 macOS 客户端，不进 core）

- 纯音频、按进程捕获：优先评估 **Core Audio Process Tap**。
- 需要屏幕内容或沿用现有捕获流程：**ScreenCaptureKit**。
- 谁更省电靠实测，不能只凭 API 层级判断。

## 测试基准（所有后端对比都按这套执行）

相同音频、相同质量要求，比较：**首条字幕延迟、最终译文延迟（p50/p95）、识别错误率、译文质量、持续功耗、总内存**。场景覆盖：冷启动、长时间播放、GPU 同时被其他应用使用。系统服务（SpeechAnalyzer / Translation 由系统托管）的耗电与内存也要计入，不能只看主进程。

"效率" = 字幕延迟 + 准确率 + 持续功耗 + 与播放器/其他应用的资源竞争。单次识别最快 ≠ 最适合长期后台监听。

## 可用性检查纪律（Apple 原生框架）

不要因为"Apple 官方"就假设它在所有语言、所有 M4 设备上最快、最准、最省电。运行时必须动态检查：

- SpeechAnalyzer / SpeechTranscriber：`isAvailable`、`supportedLocales`、`AssetInventory` 的模型资产状态；模型升级由系统控制。
- Translation：`LanguageAvailability` 按实际语言对查询；首次使用可能需要下载语言资产。

公开 API 不承诺内部 CPU/GPU/ANE 调度细节。

## 何时读参考文件

实际实现 SpeechAnalyzer 或 Translation 后端之前，先读 `references/apple-frameworks.md`（版本要求、API 形态、资产管理、与现有管道的对接方式）。
