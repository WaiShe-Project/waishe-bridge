; WaiShe Bridge installer. Build with scripts\build-release.ps1, which passes /DAppVersion.
; Per-user install, no administrator rights (APP-01); Windows 11 x64 only (APP-09).
; No drivers are bundled (DRV-01).

#ifndef AppVersion
  #error Pass /DAppVersion=x.y.z
#endif

[Setup]
AppId={{4D6073A6-8D74-433B-8F08-7D9A90131E1A}
AppName=WaiShe
AppVersion={#AppVersion}
AppVerName=WaiShe {#AppVersion}
AppPublisher=The WaiShe Project
AppPublisherURL=https://github.com/WaiShe-Project
AppSupportURL=https://github.com/WaiShe-Project/waishe-bridge/issues
DefaultDirName={localappdata}\Programs\WaiShe
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 11
MinVersion=10.0.22000
OutputDir=..\..\target\installer
OutputBaseFilename=WaiShe-Setup-{#AppVersion}-x64
Compression=lzma2/max
SolidCompression=yes
CloseApplications=yes
RestartApplications=no
UninstallDisplayIcon={app}\waishe-bridge.exe
UninstallDisplayName=WaiShe
WizardStyle=modern
ShowLanguageDialog=auto

[Languages]
Name: "chinesesimp"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
chinesesimp.LaunchNow=启动 WaiShe
english.LaunchNow=Start WaiShe
chinesesimp.DeleteUserData=是否同时删除 WaiShe 的设置、已安装的驱动和日志？%n%n选择“否”会保留它们，重新安装后可继续使用。
english.DeleteUserData=Also delete WaiShe settings, installed drivers and logs?%n%nChoose No to keep them for a later reinstall.

[Files]
Source: "..\..\target\release\waishe-bridge.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; The AppUserModelID lets Windows show Bridge's notifications under the WaiShe name.
Name: "{autoprograms}\WaiShe"; Filename: "{app}\waishe-bridge.exe"; AppUserModelID: "WaiShe.Bridge"

[Registry]
; Bridge writes this value itself when start at sign-in is on; remove it on uninstall (APP-05).
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "WaiShe"; Flags: uninsdeletevalue dontcreatekey

[Run]
Filename: "{app}\waishe-bridge.exe"; Description: "{cm:LaunchNow}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM waishe-bridge.exe"; Flags: runhidden; RunOnceId: "StopBridge"

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usPostUninstall) and not UninstallSilent then
    if MsgBox(CustomMessage('DeleteUserData'), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
    begin
      DelTree(ExpandConstant('{userappdata}\WaiShe'), True, True, True);
      DelTree(ExpandConstant('{localappdata}\WaiShe'), True, True, True);
    end;
end;
