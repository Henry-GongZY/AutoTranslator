param(
    # Engines to bundle; cpu always ships. CUDA/VULKAN need their toolkits on
    # the build machine only - the target machine needs just a driver.
    [ValidateSet('cpu', 'blas', 'vulkan', 'cuda')]
    [string[]]$Engine = @('cpu', 'cuda'),
    [string]$Configuration = 'Release'
)
$ErrorActionPreference = 'Stop'
# Goal: one publish output that runs on any Windows 10 2004+ x64 machine with
# zero installs - .NET runtime, Windows App SDK and the Rust engines all ship
# inside the folder.

$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo

# 1. Engine packages (each an isolated exe + native deps layout).
& powershell -NoProfile -ExecutionPolicy Bypass -File "$repo\scripts\build-engines.ps1" -Engine $Engine
if ($LASTEXITCODE -ne 0) { throw 'engine build failed' }

# 2. Publish the WinUI client fully self-contained:
#    -SelfContained   bundles the .NET runtime (target needs no dotnet install)
#    WindowsAppSDKSelfContained is already on in the csproj, so the WinAppSDK
#    runtime is bundled too (target needs no runtime installer).
$publishDir = "$repo\target\windows-portable\Translator.Desktop"
dotnet publish "$repo\clients\windows\Translator.Desktop\Translator.Desktop.csproj" `
    -c $Configuration -r win-x64 --self-contained true `
    -p:PublishDir="$publishDir\"
if ($LASTEXITCODE -ne 0) { throw 'dotnet publish failed' }

# 3. Ship the VC++ runtime DLLs next to every engine: whisper.cpp's MSVC build
#    needs vcruntime140/msvcp140, which are NOT guaranteed on a clean Windows.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ($vs) {
    $crt = Get-ChildItem "$vs\VC\Redist\MSVC\*\x64\Microsoft.VC143.CRT" -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending | Select-Object -First 1
    if ($crt) {
        foreach ($engine in $Engine) {
            $dest = "$repo\engines\$engine"
            New-Item -ItemType Directory -Force $dest | Out-Null
            Copy-Item "$crt\*.dll" $dest -Force
        }
    }
    else { Write-Warning 'VC++ redistributable DLLs not found; engines may need the VC redist installed on targets.' }
}

# 4. Sanity: every bundled engine must report its id.
foreach ($engine in $Engine) {
    $actual = & "$repo\engines\$engine\translator-core.exe" --engine-info
    if ($actual -ne $engine) { throw "engine verification failed: $engine ($actual)" }
}

# 5. Zip the portable folder. Run Translator.Desktop.exe from anywhere.
$distRoot = "$repo\target\windows-portable"
$zip = "$repo\target\Translator-windows-x64-portable.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path "$distRoot\*" -DestinationPath $zip -Force

Write-Host "Portable build ready: $zip"
Write-Host 'Targets: Windows 10 2004+ x64 (or Windows 11), no runtime installs required.'
Write-Host 'GPU engines additionally need the matching driver on the target (CUDA: NVIDIA driver; Vulkan: any GPU driver).'
