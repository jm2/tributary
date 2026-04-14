; Tributary — Inno Setup installer script
; Used by: scripts/build-windows.ps1 -InnoSetup
;
; Preprocessor defines (passed via /D on the iscc command line):
;   AppVersion   — e.g. "0.1.0"
;   SourceDir    — path to the bundled dist folder (dist\tributary-windows)
;   OutputDir    — where to write the installer exe
;   TargetArch   — "x64" or "arm64"

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\dist\tributary-windows"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif
#ifndef TargetArch
  #define TargetArch "x64"
#endif

[Setup]
AppName=Tributary
AppVersion={#AppVersion}
AppId={{E8A3B2F1-7C4D-4E5A-9F6B-1D2E3F4A5B6C}
VersionInfoVersion={#AppVersion}
AppPublisher=Tributary Contributors
AppPublisherURL=https://github.com/jm2/tributary
AppSupportURL=https://github.com/jm2/tributary/issues
AppUpdatesURL=https://github.com/jm2/tributary/releases
DefaultDirName={autopf}\Tributary
DefaultGroupName=Tributary
UninstallDisplayIcon={app}\tributary.exe
OutputDir={#OutputDir}
OutputBaseFilename=tributary-setup
Compression=lzma2/ultra64
SolidCompression=yes
SetupIconFile=..\..\data\tributary.ico
LicenseFile=..\..\LICENSE
WizardStyle=modern
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=commandline
; Silent install support (Winget passes /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-)
DisableDirPage=auto
DisableProgramGroupPage=auto
CloseApplications=yes
CloseApplicationsFilter=tributary.exe
SetupLogging=yes
#if TargetArch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; Copy everything from the bundled dist directory
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\Tributary"; Filename: "{app}\tributary.exe"
Name: "{group}\Uninstall Tributary"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Tributary"; Filename: "{app}\tributary.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\tributary.exe"; Description: "Launch Tributary"; Flags: nowait postinstall skipifnotsilent
