@echo off
REM Build translator-core with the whisper.cpp CUDA backend (GPU) using Ninja.
REM Ninja + nvcc + cl needs no CUDA<->Visual Studio integration. The cmake crate
REM otherwise injects a VS "instance specification" that Ninja rejects, so we
REM clear the VS env vars that trigger it (cl.exe/lib paths stay on PATH/INCLUDE/LIB).
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat" x64
set PATH=C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6\bin;C:\Users\Henrygongzy\bin;%PATH%
set WHISPER_CUDA=1
set CMAKE_GENERATOR=Ninja
set CMAKE_GENERATOR_INSTANCE=
set CMAKE_GENERATOR_TOOLSET=
set CMAKE_GENERATOR_PLATFORM=
set VSINSTALLDIR=
set VCToolsInstallDir=
set WindowsSdkDir=
set PROTOC=C:\Users\Henrygongzy\AppData\Local\Microsoft\WinGet\Packages\Google.Protobuf_Microsoft.Winget.Source_8wekyb3d8bbwe\bin\protoc.exe
cd /d c:\Users\Henrygongzy\Desktop\Projects\Tools\Translator
for /d %%d in (target\release\build\whisper-rs-sys-*) do rmdir /s /q "%%d"
cargo build -p translator-core --features cuda --release > build_core_gpu.log 2>&1
echo CARGO_EXIT=%ERRORLEVEL% >> build_core_gpu.log
