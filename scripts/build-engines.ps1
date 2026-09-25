param(
    [ValidateSet('cpu','blas','vulkan','cuda')][string[]]$Engine = @('cpu','blas','vulkan','cuda'),
    [switch]$Test
)
$ErrorActionPreference = 'Stop'
# Separate processes prevent CUDA's VS2022 environment leaking into other builds.
if ($Engine.Count -gt 1) {
    foreach ($backend in $Engine) {
        $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $PSCommandPath, '-Engine', $backend)
        if ($Test) { $arguments += '-Test' }
        & powershell.exe @arguments
        if ($LASTEXITCODE -ne 0) { throw "$backend build failed" }
    }
    return
}
$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = if ($Engine[0] -eq 'cuda') {
    & $vswhere -version '[17.0,18.0)' -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
} else {
    & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
}
if (!$vs) { throw 'Install Visual Studio 2022 C++ Build Tools.' }
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvarsall.bat'
# Import the developer environment without changing permanent user settings.
$environmentScript = Join-Path ([IO.Path]::GetTempPath()) ("translator-vcvars-" + [guid]::NewGuid().ToString('N') + '.cmd')
try {
    @("@call `"$vcvars`" x64 >nul", '@if errorlevel 1 exit /b 1', '@set') | Set-Content -LiteralPath $environmentScript -Encoding ascii
    $environmentLines = & cmd.exe /d /c $environmentScript
    if ($LASTEXITCODE -ne 0) { throw 'VS2022 environment initialization failed.' }
    $environmentLines | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') { [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process') }
    }
} finally { Remove-Item -LiteralPath $environmentScript -ErrorAction SilentlyContinue }
if (!$env:OPENBLAS_PATH -and (Test-Path "$repo\.build-deps\openblas")) { $env:OPENBLAS_PATH = "$repo\.build-deps\openblas" }
if (!$env:VULKAN_SDK -and (Test-Path "$repo\.build-deps\vulkan")) { $env:VULKAN_SDK = "$repo\.build-deps\vulkan" }
$env:PATH = "$env:USERPROFILE\bin;$env:CUDA_PATH\bin;$env:PATH"
$env:CMAKE_GENERATOR = 'Ninja'
'CMAKE_GENERATOR_INSTANCE','CMAKE_GENERATOR_TOOLSET','CMAKE_GENERATOR_PLATFORM','VSINSTALLDIR','VCToolsInstallDir','WindowsSdkDir' | ForEach-Object { Remove-Item "Env:$_" -ErrorAction SilentlyContinue }
if (!(Get-Command protoc -ErrorAction SilentlyContinue)) {
    $protoc = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\Google.Protobuf*\bin\protoc.exe" | Select-Object -First 1
    if (!$protoc) { throw 'Install protoc and put it on PATH.' }
    $env:PROTOC = $protoc.FullName
}
foreach ($backend in $Engine) {
    if ($backend -eq 'cuda' -and !(Test-Path "$env:CUDA_PATH\bin\nvcc.exe")) { throw 'CUDA_PATH must point to CUDA Toolkit 12.6.' }
    if ($backend -eq 'vulkan' -and !(Test-Path "$env:VULKAN_SDK\Lib\vulkan-1.lib")) { throw 'Install Vulkan SDK and set VULKAN_SDK.' }
    if ($backend -eq 'blas') {
        if (!(Test-Path "$env:OPENBLAS_PATH\lib\libopenblas.lib")) { throw 'OPENBLAS_PATH must contain lib/libopenblas.lib and include/cblas.h.' }
        $env:PATH = "$env:OPENBLAS_PATH\bin;$env:PATH"
        $env:BLAS_INCLUDE_DIRS = "$env:OPENBLAS_PATH\include"
        $env:CMAKE_PREFIX_PATH = $env:OPENBLAS_PATH
    }
    $env:WHISPER_CUDA = if ($backend -eq 'cuda') { '1' } else { '0' }
    $features = if ($backend -eq 'cpu') { 'whisper' } else { $backend }
    & cargo build -p translator-core --release --no-default-features --features $features
    if ($LASTEXITCODE -ne 0) { throw "$backend build failed" }
    $dest = Join-Path $repo "engines\$backend"
    New-Item -ItemType Directory -Force $dest | Out-Null
    Copy-Item -LiteralPath "$repo\target\release\translator-core.exe" -Destination $dest -Force
    if ($backend -eq 'blas') { Get-ChildItem "$env:OPENBLAS_PATH\bin\*.dll" | Copy-Item -Destination $dest -Force }
    $actual = & "$dest\translator-core.exe" --engine-info
    if ($LASTEXITCODE -ne 0 -or $actual -ne $backend) { throw "Engine verification failed: $backend ($actual)" }
    if ($Test) {
        & cargo test -p translator-core --release --no-default-features --features $features --lib --test e2e
        if ($LASTEXITCODE -ne 0) { throw "$backend tests failed" }
    }
}
Write-Host 'Engine packages are ready. Run dotnet build -c Release for the desktop project.'
