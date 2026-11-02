; Inno Setup 6 script for the OpenXCloud installer. Built by scripts/package-windows.ps1,
; which passes the defines below; see PACKAGING.md.
;
;   AppVersion      display version, e.g. 0.2.0-beta.1
;   NumericVersion  four-part file version, e.g. 0.2.0.0
;   StageDir        folder with openxcloud.exe, its DLLs and licences
;   RepoDir         repository root (licence, icon)
;   OutputDir       where the setup .exe goes
;   OutputName      setup file name without .exe

#ifndef AppVersion
  #error AppVersion must be defined (run scripts/package-windows.ps1)
#endif

[Setup]
; Never change AppId: upgrades and uninstall find earlier installs through it.
AppId={{FDB333F5-704E-4E86-988E-2D383F8A2711}
AppName=OpenXCloud
AppVersion={#AppVersion}
AppVerName=OpenXCloud {#AppVersion}
AppPublisher=OpenXCloud contributors
AppPublisherURL=https://github.com/shyim/openxcloud
AppSupportURL=https://github.com/shyim/openxcloud/issues
AppUpdatesURL=https://github.com/shyim/openxcloud/releases
VersionInfoVersion={#NumericVersion}
VersionInfoProductName=OpenXCloud
VersionInfoDescription=OpenXCloud Setup

; Per-user install without admin rights by default; the dialog offers all users.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={autopf}\OpenXCloud
DefaultGroupName=OpenXCloud
DisableProgramGroupPage=yes
UsePreviousAppDir=yes

; 64-bit only; Windows 10 1809 or newer (D3D11.4 fences and D3D12 for the video path).
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763

; The licence page shows the GPL for information; it is not an agreement to accept.
LicenseFile={#RepoDir}\LICENSE
SetupIconFile={#RepoDir}\assets\icons\openxcloud.ico
UninstallDisplayIcon={app}\openxcloud.exe
UninstallDisplayName=OpenXCloud
WizardStyle=modern
OutputDir={#OutputDir}
OutputBaseFilename={#OutputName}
Compression=lzma2/ultra64
SolidCompression=yes
; Close a running OpenXCloud (Restart Manager) before replacing its files.
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[InstallDelete]
; DLLs of an older FFmpeg major (e.g. avcodec-62.dll) would otherwise stay behind.
Type: files; Name: "{app}\av*.dll"
Type: files; Name: "{app}\sw*.dll"

[Icons]
Name: "{autoprograms}\OpenXCloud"; Filename: "{app}\openxcloud.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\OpenXCloud"; Filename: "{app}\openxcloud.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\openxcloud.exe"; Description: "{cm:LaunchProgram,OpenXCloud}"; Flags: nowait postinstall skipifsilent

; Settings, tokens and logs live in the user's AppData and the Windows credential store;
; uninstalling keeps them, like most desktop apps, so a reinstall stays signed in.
