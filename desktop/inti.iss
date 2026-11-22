; Inti for Windows - per-user installer (no admin rights). build.py compiles it with:
;   iscc /DAppVersion=1.0.0 /DSourceDir=<dist\Inti> /DOutputDir=<dist> /DIconFile=<build\inti.ico> inti.iss
; Installs to %LOCALAPPDATA%\Programs\Inti, adds Inti to the Start menu (and optionally the desktop) and lists it
; in Settings > Apps. The window keeps its data (what you saved in it) in %LOCALAPPDATA%\Inti, which
; uninstalling leaves in place so a reinstall finds it again.

#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "dist\Inti"
#endif
#ifndef OutputDir
  #define OutputDir "dist"
#endif
#ifndef IconFile
  #define IconFile "build\inti.ico"
#endif

[Setup]
AppId={{E81247BA-698B-569E-A6DE-E1E1C366EEFB}
AppName=Inti
AppVersion={#AppVersion}
AppVerName=Inti {#AppVersion}
AppPublisher=Juwen Lie
AppPublisherURL=https://juw12.github.io/inti/
AppSupportURL=https://github.com/juw12/inti
DefaultDirName={autopf}\Inti
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir={#OutputDir}
OutputBaseFilename=Inti-Windows-Setup
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\Inti.exe
UninstallDisplayName=Inti
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=no
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{userprograms}\Inti"; Filename: "{app}\Inti.exe"; Comment: "Inti is a small operating system written from scratch"
Name: "{userdesktop}\Inti"; Filename: "{app}\Inti.exe"; Comment: "Inti is a small operating system written from scratch"; Tasks: desktopicon

[Run]
Filename: "{app}\Inti.exe"; Description: "{cm:LaunchProgram,Inti}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\Inti.exe"; Parameters: "--stop"; Flags: runhidden waituntilterminated; RunOnceId: "StopInti"

[Code]
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  Result := '';
  { Updating: close an open Inti window first so its files can be replaced. }
  if FileExists(ExpandConstant('{app}\Inti.exe')) then
    Exec(ExpandConstant('{app}\Inti.exe'), '--stop', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;
