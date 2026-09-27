; AutoTranslator Windows installer (Inno Setup 6).
; Packs the self-contained publish output produced by
; scripts/build-windows-portable.ps1 (dotnet runtime + WinAppSDK + engines).
; Per-user install by default: no admin rights, like modern desktop apps.
;
; Build:  iscc packaging/translator.iss /DAppVersion=x.y.z
; Input:  target/windows-portable/Translator.Desktop  (publish folder)

#ifndef AppVersion
#define AppVersion "0.0.0"
#endif

[Setup]
AppId={{7C1E2A34-5B6D-4E78-9A0B-C2D3E4F50617}
AppName=AutoTranslator
AppVersion={#AppVersion}
AppPublisher=AutoTranslator
DefaultDirName={autopf}\AutoTranslator
DefaultGroupName=AutoTranslator
DisableProgramGroupPage=yes
OutputDir=..\target\installer
OutputBaseFilename=AutoTranslatorSetup-x64-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
LZMAUseSeparateProcess=yes
ArchitecturesInstallIn64BitMode=x64compatible
; Per-user: {autopf} resolves to %LOCALAPPDATA%\Programs, no elevation needed.
PrivilegesRequired=lowest
UninstallDisplayIcon={app}\Translator.Desktop.exe
WizardStyle=modern

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式(&D)"; GroupDescription: "附加图标:"

[Files]
; The whole publish tree: app exe + runtime + engines\<backend>\...
Source: "..\target\windows-portable\Translator.Desktop\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion

[Icons]
Name: "{group}\AutoTranslator"; Filename: "{app}\Translator.Desktop.exe"
Name: "{autodesktop}\AutoTranslator"; Filename: "{app}\Translator.Desktop.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\Translator.Desktop.exe"; Description: "启动 AutoTranslator"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Downloaded whisper models live in a user-chosen folder (default under the
; install dir) — remove the in-tree copy but never touch user folders.
Type: filesandordirs; Name: "{app}\engines"
