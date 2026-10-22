; ReMIDI Inno Setup Script
; Generates remidi-setup.exe for Windows 64-bit

#define MyAppName "ReMIDI"
#define MyAppVersion "0.3.0"
#define MyAppPublisher "AlienSys"
#define MyAppURL "https://github.com/aliensys/remidi"
#define MyAppExeName "remidi.exe"

[Setup]
AppId={{8B1A246B-4811-4E62-9DDC-801D11B55B49}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
AllowNoIcons=yes
OutputDir=..\target\installer
OutputBaseFilename=remidi-setup
SetupIconFile=..\assets\remidi.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible
PrivilegesRequiredOverridesAllowed=commandline dialog
CloseApplications=yes
CloseApplicationsFilter=remidi.exe

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "french"; MessagesFile: "compiler:Languages\French.isl"

[CustomMessages]
english.StartupTask=Launch ReMIDI automatically when Windows starts
french.StartupTask=Lancer ReMIDI automatiquement au démarrage de Windows
english.DriverNoticeMessage=The teVirtualMIDI driver was not detected in System32.%n%nReMIDI requires teVirtualMIDI to create virtual MIDI loopback ports on Windows.%n%nWould you like to open the official teVirtualMIDI download page now?
french.DriverNoticeMessage=Le pilote teVirtualMIDI n'a pas été détecté dans System32.%n%nReMIDI nécessite ce pilote pour créer des ports MIDI virtuels sous Windows.%n%nSouhaitez-vous ouvrir la page officielle de téléchargement de teVirtualMIDI maintenant ?
english.UninstallCleanConfig=Do you also want to remove your ReMIDI configuration and logs (%APPDATA%\remidi)?
french.UninstallCleanConfig=Voulez-vous également supprimer la configuration et les logs de ReMIDI (%APPDATA%\remidi) ?
english.OpenConfigFolder=ReMIDI Configuration Folder
french.OpenConfigFolder=Dossier de configuration ReMIDI
english.OpenDefaultConfig=View default template (config.default.toml)
french.OpenDefaultConfig=Consulter le modèle de configuration (config.default.toml)

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "startup"; Description: "{cm:StartupTask}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\config.default.toml"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\assets\remidi.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\remidi.ico"
Name: "{group}\{cm:OpenDefaultConfig}"; Filename: "{app}\config.default.toml"
Name: "{group}\{cm:OpenConfigFolder}"; Filename: "{userappdata}\remidi"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\remidi.ico"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#MyAppName}"; ValueData: """{app}\{#MyAppExeName}"""; Tasks: startup; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: nowait postinstall skipifsilent

[Code]
function CheckForDriver(): Boolean;
begin
  Result := FileExists(ExpandConstant('{sys}\teVirtualMIDI64.dll')) or
            FileExists(ExpandConstant('{sys}\teVirtualMIDI32.dll'));
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ErrorCode: Integer;
begin
  if CurStep = ssPostInstall then
  begin
    if not CheckForDriver() then
    begin
      if SuppressibleMsgBox(CustomMessage('DriverNoticeMessage'), mbInformation, MB_YESNO, IDYES) = IDYES then
      begin
        ShellExec('open', 'https://www.tobias-erichsen.de/software/tevirtualmidi.html', '', '', SW_SHOWNORMAL, ewNoWait, ErrorCode);
      end;
    end;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    if DirExists(ExpandConstant('{userappdata}\remidi')) then
    begin
      if SuppressibleMsgBox(CustomMessage('UninstallCleanConfig'), mbConfirmation, MB_YESNO, IDNO) = IDYES then
      begin
        DelTree(ExpandConstant('{userappdata}\remidi'), True, True, True);
      end;
    end;
  end;
end;
