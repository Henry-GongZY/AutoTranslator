# 部署与兼容性（一次构建，到处运行）

"一次构建到处运行"的含义：**构建机**需要编译环境（编译型语言的必然），但**产物**不依赖目标机安装任何运行时/SDK。

| 平台 | 现状 | 结论 |
| --- | --- | --- |
| Windows | `scripts/build-windows-portable.ps1` | ✅ 已实现：产物自带 .NET 运行时 + Windows App SDK + 引擎，目标机零安装 |
| macOS | 当前产物 target=macOS 15、arm64-only | ⚠️ 只在 Apple Silicon + macOS 15+ 可跑；按下方清单改造为 floor 13 通用二进制 |

## GitHub Actions 自动打包（.github/workflows/release.yml）

- **触发**：推送 `v*` tag → 构建双平台安装包并发布 GitHub Release；`workflow_dispatch` 手动构建仅产出 artifacts
- **Windows**（windows-latest）：cpu+vulkan 引擎（CUDA 由手动开关加装 toolkit 12.6）→ `dotnet publish --self-contained` → VC CRT 复制 → `packaging/translator.iss`（Inno Setup，用户级安装免管理员）产出 `AutoTranslatorSetup-x64-<版本>.exe` + 便携 zip
- **macOS**（macos-15, arm64）：metal 引擎 → 桥接 → App bundle →（配置 secrets 后）Developer ID 签名 + notarytool 公证 + staple → `hdiutil` 打包 `AutoTranslator-macos-arm64-<版本>.dmg`（拖入 Applications 式标准分发）
- **Release**：tag 构建自动附带 SHA256SUMS

macOS 签名/公证所需 secrets（不配置则产出未签名 DMG，用户首次打开需右键→打开）：

| Secret | 内容 |
| --- | --- |
| MACOS_CERTIFICATE_P12 | Developer ID Application 证书 .p12 的 base64 |
| MACOS_CERTIFICATE_PASSWORD | .p12 导出密码 |
| APPLE_ID / APPLE_PASSWORD / APPLE_TEAM_ID | notarytool 凭据（Apple ID + App 专用密码 + 团队 ID） |

Windows 代码签名未配置：安装器无签名会有 SmartScreen 提示（点"仍要运行"），后续可加 EV 证书签名步骤。

## Windows（已实现）

构建（需要在装了 VS2022 Build Tools + .NET 10 SDK + CUDA/Vulkan SDK 的机器上执行一次）：

```powershell
scripts/build-windows-portable.ps1                      # cpu + cuda
scripts/build-windows-portable.ps1 -Engine cpu,vulkan   # 自选引擎组合
```

产物：`target/Translator-windows-x64-portable.zip`，解压即用。

- **目标机要求**：Windows 10 2004 (build 19041) 及以上，x64（ARM64 走 x64 模拟，未做原生 arm64 引擎包）。
- **目标机不需要**：.NET SDK/运行时（self-contained 打包）、Windows App SDK 运行时（`WindowsAppSDKSelfContained`）、VC++ 运行库（脚本把 VC143 CRT DLL 复制进每个引擎目录）。
- **GPU 引擎额外条件**（只与驱动有关，与系统版本无关）：`cuda` 需要 NVIDIA 驱动；`vulkan` 需要任意现代 GPU 驱动（加载器随包）；`cpu`/`blas` 无要求。
- 采集：WASAPI loopback（NAudio），Windows 7 时代 API，无高版本依赖。
- 未打包引擎的后端：UI 自动回退（无 CUDA 包则 CPU 引擎兜底）。

## macOS（改造清单 + 兼容性矩阵）

### 现状判定（2026-09）

- 构建机：macOS 15.7 + CommandLineTools（SDK 26），产物默认 target = 构建机系统（15.0）、**仅 arm64**、`LSMinimumSystemVersion=15.0`。
- 功能决定的地板：**Translation 框架要求 macOS 15.0+**。所以今天的"15+"不只是构建参数，还是功能边界。

### 一次构建到处运行的改造清单

1. **选定部署地板**。建议 **macOS 13 Ventura**：同时覆盖 Intel Mac 与 Apple Silicon，且 ScreenCaptureKit 音频采集（13.0+）可用。愿意放弃老 Intel 可选 14/15，改造更少。
2. **部署目标三件套**（构建脚本参数化）：
   - `swiftc -target arm64-apple-macos13.0`（x86_64 同理；两个架构产物 `lipo -create` 成 Universal）
   - Rust core：`MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --target aarch64-apple-darwin`（x86_64 同理，lipo）
   - `Info.plist` 的 `LSMinimumSystemVersion` 改 13.0
3. **API 可用性门控**：所有高于地板的 API 用 `if #available(macOS 15, *)` 包裹 + UI 按系统版本隐藏功能（如 13/14 上翻译入口显示"需要 macOS 15"）。
4. **跨机器分发**：现在的 **Apple Development 证书只在本机/注册设备有效**。分发给别人需要 Developer ID Application 证书 + Hardened Runtime + 公证（`notarytool` + staple）。TCC（屏幕录制）在每台新机器上首次授权，证书稳定签名（见 docs/macos-client.md）保证重编译不失效。
5. **Swift 运行时**：macOS 13 自带 Swift 5.7 ABI 运行时，新工具链编译的代码走 back-deployment；最稳妥是 `swiftc -static-stdlib`（把 Swift 标准库静态链入）并在最老系统实机验证。
6. **模型分发**：whisper 模型首次会话联网下载；要完全离线可把 ggml 模型预置进 bundle（代价是包体 +75MB/模型）。

### Feature × 系统版本矩阵

| Feature | 最低系统 | 旧系统行为 |
| --- | --- | --- |
| WASAPI loopback 采集（Windows 对照项） | Win7+ | — |
| ScreenCaptureKit 采集（视频流） | 12.3+ | — |
| **SCK 音频采集**（本项目实际依赖） | **13.0+** | 12 及以下无系统音频路径 |
| whisper.cpp Metal 推理 | 13+（ggml 运行时加载着色器，随 Metal 能力而定） | — |
| Translation 框架（apple-translate 后端） | **15.0+** | 隐藏入口 / 状态上报"系统不可用" |
| Translation 直接初始化（无 SwiftUI、进程内下载） | 26.0+ | 回退 15 的隐藏窗口方案（已实现） |
| SpeechAnalyzer / SpeechTranscriber | **26.0+** | Whisper Metal 兜底（路线图既定回退）；已在 macOS 27 实机端到端验证 |
| Core Audio Process Tap（采集备选） | 14.4+ | SCK |
| Foundation Models（术语润色等） | 26.0+ | 不提供 |
| Swift `onChange(of:)` 双参、`MainActor.assumeIsolated` | 14.0 | floor=13 时须回退单参/`dispatchPrecondition` |

### 新系统上可升级为更高效实现的部分

- **macOS 26**：SpeechAnalyzer/SpeechTranscriber —— 流式识别 + 系统托管模型，替换"12 秒窗口每 2 秒重解码"的延迟/功耗来源（路线图第 3 步的既定候选）。
- **macOS 26**：Translation `TranslationSession(installedSource:target:)` 直接初始化 + `canRequestDownloads`，资产下载交互完全进程内完成——可精简掉 15 的隐藏窗口 fallback，下载提示不再依赖客户端 UI。
- **macOS 14.4+**：Core Audio Process Tap —— 纯音频按进程捕获的备选，与 SCK 的功耗/延迟对比是 skill 里的实测项。
- **macOS 26**：Foundation Models —— 术语修正、上下文润色（非实时路径）。
- **Core ML / ANE**：whisper encoder 加速（路线图第 4 步），由硬件而非 OS 版本决定。

### 验证方法

- macOS 无法模拟旧系统行为，`#available` 门控必须在**最老支持系统的实机/VM**上验证一遍。
- 每个被门控的 feature 记录降级路径：Translation → 提示需 15；SpeechAnalyzer → Whisper Metal；Process Tap → SCK。
- Windows 便携包在干净 Win10 2004 虚拟机上跑一遍（无 dotnet、无 VC redist、无 GPU 驱动 → cpu 引擎应可用）。
