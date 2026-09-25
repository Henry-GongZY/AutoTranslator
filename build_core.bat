@echo off
call "C:\Program Files\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvarsall.bat" x64
set PATH=C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6\bin;%PATH%
set PROTOC=C:\Users\Henrygongzy\AppData\Local\Microsoft\WinGet\Packages\Google.Protobuf_Microsoft.Winget.Source_8wekyb3d8bbwe\bin\protoc.exe
cd /d c:\Users\Henrygongzy\Desktop\Projects\Tools\Translator
cargo build -p translator-core --release > build_core.log 2>&1
if not errorlevel 1 (
    REM Deploy the fresh core into every client output dir; the csproj only
    REM re-copies on a .NET rebuild.
    for /d %%d in ("%~dp0clients\windows\Translator.Desktop\bin\*\*\win-x64\core") do (
        copy /y "%~dp0target\release\translator-core.exe" "%%d\translator-core.exe" >nul
        echo deployed core to %%d >> build_core.log
    )
)
echo CARGO_EXIT=%ERRORLEVEL% >> build_core.log
