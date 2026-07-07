; tty7 Windows installer (Inno Setup 6 — preinstalled on GitHub's
; windows-latest runners). Compiled by bundle-windows.ps1, which stages the
; payload and passes every path in via /D defines:
;
;   /DAppVersion=<semver>   version parsed from Cargo.toml
;   /DStageDir=<abs path>   staged payload (tty7.exe, completions\, LICENSE.txt, README.md)
;   /DOutputDir=<abs path>  where the setup exe is written
;   /DOutputName=<basename> setup exe filename, without ".exe"
;
; Defaults to a per-user install ({localappdata}\Programs\tty7 — no UAC
; prompt), with an "install for all users" escape hatch in the dialog. The
; build is unsigned, so SmartScreen warns on first launch either way — same as
; the portable zip.

#ifndef AppVersion
  #error Missing /DAppVersion — this script is meant to be compiled via bundle-windows.ps1
#endif

[Setup]
; Never change AppId: it is how Windows ties upgrades + the uninstall entry
; to previous installs of tty7.
AppId={{9A3F6C1E-4B7D-4E2A-8C5F-D01B92E64A37}
AppName=tty7
AppVersion={#AppVersion}
AppPublisher=tty7 contributors
AppPublisherURL=https://github.com/l0ng-ai/tty7
AppSupportURL=https://github.com/l0ng-ai/tty7/issues
AppUpdatesURL=https://github.com/l0ng-ai/tty7/releases
DefaultDirName={autopf}\tty7
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
LicenseFile={#StageDir}\LICENSE.txt
SetupIconFile=..\..\assets\favicon.ico
UninstallDisplayIcon={app}\tty7.exe
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#StageDir}\tty7.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\completions\*"; DestDir: "{app}\completions"; Flags: ignoreversion recursesubdirs
Source: "{#StageDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\tty7"; Filename: "{app}\tty7.exe"
Name: "{autodesktop}\tty7"; Filename: "{app}\tty7.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\tty7.exe"; Description: "{cm:LaunchProgram,tty7}"; Flags: nowait postinstall skipifsilent
