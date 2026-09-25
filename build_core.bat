@echo off
call "C:\Program Files\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvarsall.bat" x64
set PATH=C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6\bin;%PATH%
set PROTOC=C:\Users\Henrygongzy\AppData\Local\Microsoft\WinGet\Packages\Google.Protobuf_Microsoft.Winget.Source_8wekyb3d8bbwe\bin\protoc.exe
cd /d c:\Users\Henrygongzy\Desktop\Projects\Tools\Translator
cargo build -p translator-core --release > build_core.log 2>&1
echo CARGO_EXIT=%ERRORLEVEL% >> build_core.log
