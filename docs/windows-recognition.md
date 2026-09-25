# Windows 多引擎识别

## 使用

识别服务与计算引擎分开：选择「本地 · Whisper」，然后选择 CPU、BLAS、Vulkan 或 CUDA，以及模型和音频语言。

- CPU：通用 CPU 后端。
- BLAS：OpenBLAS 加速的 CPU 后端，不是 GPU 后端。
- Vulkan：支持具备兼容驱动的 NVIDIA、AMD、Intel GPU。
- CUDA：支持 NVIDIA GPU，需要对应 CUDA 运行库。

引擎按 `engines/<engine>/translator-core.exe` 独立部署。客户端将引擎、模型名、模型目录通过 `AsrConfig` 传给 core；握手能力与引擎不一致时拒绝启动。不会因缺少引擎包而静默选择另一个引擎。

模型菜单包括 tiny、base、small、medium 及各自 `.en` 英语版，以及 large、large-v1、large-v2、large-v3。`large` 是 `large-v3` 的别名，两者共用 `ggml-large-v3.bin`。英语版会自动选中英语，并限制语言选择。

选择模型或提交模型目录后，客户端检查文件并自动下载缺失模型。支持进度、取消、重试；采用流式写入、长度与 GGML 文件头检查，完成后才原子替换目标文件，异常清理临时文件。未提供模型的其他协议客户端也可由 core 下载。下载不支持断点续传；取消后再次下载会重新开始。

默认目录：`%LOCALAPPDATA%/Translator/models`。识别设置保存在 `%LOCALAPPDATA%/Translator/recognition.json`。模型目录支持浏览、手动输入绝对路径和打开。默认下载源为 hf-mirror，可通过 `TRANSLATOR_MODEL_BASE_URL` 指向兼容的模型文件目录。模型需要完整的 GGML 权重，不能用 Hugging Face 的 PyTorch 权重替代。

## 构建与发布

依赖：Rust、protoc、.NET 10、Windows SDK 10.0.26100、VS2022 C++ Build Tools、CMake、Ninja；CUDA 引擎使用 CUDA 12.6；Vulkan 引擎需要 Vulkan SDK；BLAS 引擎需要 Windows x64 LP64 OpenBLAS。

```powershell
# 必要时设置依赖目录（仅当前 PowerShell）
$env:CUDA_PATH = 'C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6'
$env:VULKAN_SDK = 'C:\VulkanSDK\你的版本'
$env:OPENBLAS_PATH = 'C:\deps\openblas'

# 四种引擎各自构建并复制到 engines/<engine>
.\scripts\build-engines.ps1

# 单个引擎及单元/管道集成测试
.\scripts\build-engines.ps1 -Engine cuda -Test

# 引擎已准备好：生成包含 .NET 运行时的完整发布目录
.\scripts\build-release.ps1 -SkipEngines

# 从引擎到桌面程序的一步构建
.\scripts\build-release.ps1
```

脚本也会识别本工作区 `.build-deps/openblas` 和 `.build-deps/vulkan` 下的构建依赖。OpenBLAS 目录需包含 `include/cblas.h`、`lib/libopenblas.lib`、`bin/libopenblas.dll`。DLL 会随 BLAS 引擎复制；CUDA 运行库仍需由目标电脑提供，客户端会将 `CUDA_PATH/bin` 加入子进程 PATH。Vulkan 运行时由显卡驱动提供。

发布产物为 `artifacts/release/Translator.Desktop.exe`。应分发整个目录。四个引擎包通过 MSBuild Content 项同时进入 build 和 publish 输出，避免旧的 CPU 核心覆盖 GPU 包。旧版 `build_core*.bat` 仅用于旧的单核心构建，新客户端请使用上述脚本。

验证下载与缓存逻辑：

```powershell
dotnet run --project tests/ModelStore.Tests/ModelStore.Tests.csproj
```

使用本地测试语音检查四种引擎的握手、错配拒绝、模型加载和识别：

```powershell
python scripts/smoke-engines.py --models 'C:\path\to\models'
```

需要 `crates/translator-core/tests/speech_test.wav`（16 kHz、单声道、16-bit PCM）。运行日志保存到 `artifacts/engine-tests`。本次已在 RTX 4060 Laptop GPU 上验证全部四种引擎；CUDA 和 Vulkan 日志均明确指向该 GPU。

## 远端 API 与其他模型的 UI 扩展

采用三层配置，避免把模型家族、硬件后端、网络服务混进同一个菜单：

1. **识别服务**：本地 Whisper、其他本地模型、远端 API、演示模式。
2. **服务专属配置**：本地 Whisper 显示四种引擎、模型目录与下载状态；其他本地模型显示适用的运行时与模型资源；远端 API 显示服务商、URL、远端模型 ID、认证与连接测试，不显示本地目录或 GPU 引擎。
3. **通用设置**：音频语言、字幕行数、显示样式等始终保留。

未来由 provider 能力描述控制面板：支持的模型、语言、运行设备、流式响应与本地资源需求。远端 API 的凭据应存入 Windows Credential Locker，不能写进当前 JSON 设置或日志；切换到远端时清楚说明音频会发往哪个服务。远端模型 ID 不应复用本地 GGML 文件名白名单。

本次只实现本地 Whisper 与 Mock。远端选项明确标记未接入并禁用，不包含假的连接按钮或未经验证的服务调用。
