; Inno Setup script for Open Download Manager on Windows.
;
; Produces a real installer: a Start-menu entry, a desktop shortcut, an
; uninstaller, and the three programs — the desktop app, the command-line
; tool, and the BitTorrent sibling. The desktop app is the default choice,
; because it starts its own daemon and needs nothing else installed.

#define AppName "Open Download Manager"
; The version comes from the command line in the release workflow
; (`ISCC /DAppVersion=0.2.0 odm.iss`), so there is one version number in the project
; rather than two that can drift apart. The value below is the fallback for a local
; build, and it is the version in `Cargo.toml`.
#ifndef AppVersion
  #define AppVersion "0.2.0"
#endif
#define AppPublisher "Open Download Manager"
#define AppExeName "odm.exe"

[Setup]
AppId={{3C1D9A44-77B2-4E2A-9F5E-0B4C6D2E7A19}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
LicenseFile=..\..\LICENSE
OutputDir=..\..\dist
OutputBaseFilename=open-download-manager-{#AppVersion}-windows-x64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible
UninstallDisplayIcon={app}\{#AppExeName}
SetupIconFile=odm.ico
ChangesAssociations=no
; The app keeps its queue under the user's profile, so it needs no elevation
; to run — but it does need it to install into Program Files.
PrivilegesRequired=admin
; …but a per-user install is offered too, because the program keeps everything it
; downloads under the user's own profile and needs nothing from Program Files.
PrivilegesRequiredOverridesAllowed=dialog

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"
Name: "addtopath"; Description: "Add odm-cli to PATH (for the command line)"; GroupDescription: "Extra:"
Name: "autostart"; Description: "Start Open Download Manager when you sign in"; GroupDescription: "Extra:"; Flags: unchecked

[Files]
; The window, the command-line tool and the BitTorrent companion.
Source: "..\..\dist\windows\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\windows\odm-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\dist\windows\odm-bt.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "odm.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; DestName: "README.md"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{group}\{#AppName} (command line)"; Filename: "{app}\odm-cli.exe"
Name: "{group}\{#AppName} (BitTorrent)"; Filename: "{app}\odm-bt.exe"
Name: "{group}\{cm:UninstallProgram,{#AppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon
; Autostart is a shortcut in the user's Startup folder rather than a registry key: it
; needs no elevation to write, and it is visible to the user, which a Run key is not.
Name: "{userstartup}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: autostart

[Registry]
; Nothing here: the app keeps its queue under the user's own profile.

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Start {#AppName} now"; \
  Flags: nowait postinstall skipifsilent

[Code]
procedure CurStepChanged(CurStep: TSetupStep);
var
  UserPath: string;
begin
  if CurStep = ssPostInstall then
  begin
    if WizardIsTaskSelected('addtopath') then
    begin
      // The user's own PATH, not the machine's: the program lives in Program Files,
      // and putting it on the machine PATH would mean elevation for every shell.
      if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', UserPath) then
        UserPath := '';
      if Pos(';' + ExpandConstant('{app}') + ';', ';' + UserPath + ';') = 0 then
      begin
        if UserPath <> '' then
          UserPath := UserPath + ';' + ExpandConstant('{app}')
        else
          UserPath := ExpandConstant('{app}');
        RegWriteStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', UserPath);
      end;
    end;
  end;
end;
