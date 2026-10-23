; Built by CI with: ISCC /DAppVersion=x.y.z assets\floki.iss
[Setup]
AppName=Floki
AppVersion={#AppVersion}
AppPublisher=Floki
DefaultDirName={localappdata}\Programs\Floki
DefaultGroupName=Floki
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
SetupIconFile=floki.ico
UninstallDisplayIcon={app}\Floki.exe
OutputDir=..\dist
OutputBaseFilename=Floki-windows-x64-setup
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
Compression=lzma2
SolidCompression=yes

[Files]
Source: "..\target\release\floki.exe"; DestDir: "{app}"; DestName: "Floki.exe"
Source: "fonts\OFL-*.txt"; DestDir: "{app}"

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"

[Icons]
Name: "{autoprograms}\Floki"; Filename: "{app}\Floki.exe"
Name: "{autodesktop}\Floki"; Filename: "{app}\Floki.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\Floki.exe"; Description: "Launch Floki"; Flags: nowait postinstall skipifsilent
