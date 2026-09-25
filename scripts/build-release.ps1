param([switch]$SkipEngines)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo
if (!$SkipEngines) { & "$PSScriptRoot\build-engines.ps1" }
foreach ($engine in 'cpu','blas','vulkan','cuda') {
    if (!(Test-Path "engines/$engine/translator-core.exe")) { throw "Missing $engine package; build it before publishing." }
}
& dotnet publish clients/windows/Translator.Desktop/Translator.Desktop.csproj -c Release -r win-x64 --self-contained true -o artifacts/release
if ($LASTEXITCODE -ne 0) { throw 'Desktop publish failed.' }
foreach ($engine in 'cpu','blas','vulkan','cuda') {
    $source = (Get-FileHash "engines/$engine/translator-core.exe").Hash
    $copy = (Get-FileHash "artifacts/release/engines/$engine/translator-core.exe").Hash
    if ($source -ne $copy) { throw "Published $engine package does not match." }
}
Write-Host 'Release ready: artifacts/release/Translator.Desktop.exe'
