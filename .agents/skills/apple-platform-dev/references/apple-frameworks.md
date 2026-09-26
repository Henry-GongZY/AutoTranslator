# Apple 框架参考（实现后端前读）

> 整理自 Apple 官方文档（2026-09）。标注"需实测"的结论来自源码与文档分析，未做性能实测；落地前按 SKILL.md 的测试基准验证。

## SpeechAnalyzer + SpeechTranscriber（macOS 26+）

- Apple 明确将它定位为**长音频、会议、低延迟实时转写**方案；模型在设备端运行，通过 `AssetInventory` 管理。
- 与本项目"持续系统音频字幕"场景高度匹配的关键能力：**直接消费音频流、异步输出识别结果**——可以避开现有"整窗重复解码"（`WINDOW_SECONDS`/`STEP_SECONDS`）方案的重复计算。
- 输入流与结果流彼此独立：这是要求演进 `SpeechRecognizer` 接口为"持续送入音频 + 独立接收识别事件"的直接原因（见 `crates/translator-core/src/asr/mod.rs:59-78`）。
- 约束：
  - 需要 macOS 26 及以上；低版本系统直接不可用，要走状态上报 + 回退 Whisper Metal。
  - 语种和模型资产可用性有约束，运行时动态检查 `isAvailable`、`supportedLocales`、资产状态。
  - 模型升级由系统控制，项目无法固定其版本。
  - 公开 API 不暴露内部 CPU/GPU/ANE 调度，"是否更快更省电"需实测，不能凭官方身份推定。

## Translation 框架（macOS 15+）

- `TranslationSession` 提供设备端翻译；第一版 macOS 文本翻译后端的首选。
- 支持情况必须按**实际语言对**通过 `LanguageAvailability` 查询。
- 首次使用可能需要下载语言资产；缺少资产时仍需相应的下载交互（模型下载交互属于 macOS 客户端的职责，见 SKILL.md 架构图）。
- macOS 26 增加了无需绑定 SwiftUI 视图的初始化方式，但前提是该语言已安装。
- 与 ASR 后端选择完全解耦：识别用 Whisper Metal 时，翻译照样可以先用 Apple Translation。

## Foundation Models（通用大模型）

- 以后可用于术语修正、上下文润色等非实时功能。
- 对持续实时字幕，先验证专门的 Translation API；不要过早引入逐 token 生成和更复杂的可用性依赖。

## whisper.cpp 的 Core ML 路径

- 主要加速 **encoder**；不是"整个识别过程都在 ANE 上"。
- 需要额外模型资产（Core ML 模型文件），打包与分发成本要计入。
- 端到端延迟必须包含：额外模型加载、首次编译、decoder 耗时。
- 文档中相对 CPU 的加速数字，不能直接当作相对 Metal 的收益；与 Metal 的对比必须实测。

## WhisperKit / MLX

- WhisperKit：第三方（非 Apple 官方），适合需要**自带可固定版本模型**并深入优化 Core ML 的场景；当前项目的非必选项。
- MLX：Apple 开源 ML 框架，面向 CPU/GPU 与统一内存；**不能把使用 MLX 等同于使用 ANE**。
- 两者都只在有明确需求时引入（路线图第 4 步之后）。

## macOS 音频采集

- **Core Audio Process Tap**：纯音频、按进程捕获的首选评估对象。
- **ScreenCaptureKit**：需要屏幕内容或沿用现有捕获流程时使用。
- 二者谁更省电需实测，不能只凭 API 层级判断；采集逻辑放在 macOS 客户端，core 只接收协商后的音频。

## 与现有 Rust 管道的对接备忘

- Swift 桥接（SpeechAnalyzer / Translation）通过本地 IPC 调用，可放客户端进程或独立辅助进程；协议消息在 `proto/` 与 `crates/translator-protocol` 中演进。
- 现有 IPC 处理循环（`crates/translator-core/src/ipc/mod.rs:54-58`）在推理期间阻塞同一连接的后续消息——接任何 Apple 后端之前先按 SKILL.md"性能守则"解耦。
- 数据量参考：48 kHz 双声道 float32 ≈ 384 KB/s；本地 IPC 不是当前瓶颈，先测再决定是否上共享内存。
