; AzurPilot — current-user installation, without global ACL/process changes.
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef OutputBaseFilename
  #define OutputBaseFilename "AzurPilot_Setup"
#endif
#ifndef PackageRoot
  #define PackageRoot "."
#endif
#ifndef SetupRoot
  #define SetupRoot "setup"
#endif
#ifndef ChineseMessagesFile
  #define ChineseMessagesFile "ChineseSimplified.isl"
#endif

[Setup]
AppName=AzurPilot
AppVersion={#AppVersion}
AppPublisher=AzurPilot Team
AppId={{1A779131-3DD5-067C-0ABC-E656396F6879}
DefaultDirName={localappdata}\Programs\AzurPilot
UsePreviousAppDir=no
AppendDefaultDirName=yes
DefaultGroupName=AzurPilot
OutputDir=.
OutputBaseFilename={#OutputBaseFilename}
Compression=lzma
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
DisableProgramGroupPage=yes
WizardStyle=modern
ShowLanguageDialog=no
MinVersion=10.0
ExtraDiskSpaceRequired=1510725938
PrivilegesRequired=lowest
UninstallDisplayIcon={app}\alas-launcher.exe
LicenseFile=LICENSE
; Restart Manager considers files registered by this installation only.
CloseApplications=yes
CloseApplicationsFilter=*.exe,*.dll,*.pyd
RestartApplications=no

[Languages]
Name: "chinesesimplified"; MessagesFile: "{#ChineseMessagesFile}"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#PackageRoot}\alas-launcher.exe"; DestDir: "{app}"; Flags: ignoreversion
; Existing user configuration always survives reinstall and uninstall.
Source: "{#PackageRoot}\config\*"; DestDir: "{app}\config"; Flags: onlyifdoesntexist uninsneveruninstall recursesubdirs createallsubdirs
Source: "{#PackageRoot}\deploy\*"; DestDir: "{app}\deploy"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#PackageRoot}\bootstrap\*"; DestDir: "{app}\bootstrap"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "{#SetupRoot}\MicrosoftEdgeWebview2Setup.exe"; DestDir: "{tmp}"; Flags: deleteafterinstall skipifsourcedoesntexist

[Icons]
Name: "{userprograms}\AzurPilot"; Filename: "{app}\alas-launcher.exe"; WorkingDir: "{app}"
Name: "{userdesktop}\AzurPilot"; Filename: "{app}\alas-launcher.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
; The bootstrapper installs WebView2 for the current user when run unelevated.
Filename: "{tmp}\MicrosoftEdgeWebview2Setup.exe"; Parameters: "/silent /install"; StatusMsg: "正在安装当前用户的 WebView2 运行环境..."; Flags: waituntilterminated skipifdoesntexist
Filename: "{app}\alas-launcher.exe"; Description: "{cm:LaunchProgram,AzurPilot}"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent

[Code]
function StartsWithPath(const Full, Prefix: String): Boolean;
var
  F, P: String;
begin
  F := RemoveBackslashUnlessRoot(ExpandFileName(Full));
  P := RemoveBackslashUnlessRoot(ExpandFileName(Prefix));
  Result := (CompareText(F, P) = 0) or
    ((Length(F) > Length(P)) and
     (CompareText(Copy(F, 1, Length(P)), P) = 0) and
     (F[Length(P) + 1] = '\'));
end;

function IsUnsafeInstallPath(const Path: String): Boolean;
var
  P: String;
begin
  P := RemoveBackslashUnlessRoot(ExpandFileName(Path));
  Result := (Length(P) <= 3) or
    (Copy(P, 1, 2) = '\\') or
    StartsWithPath(P, ExpandConstant('{win}')) or
    StartsWithPath(P, ExpandConstant('{commonpf32}')) or
    (CompareText(P, GetEnv('USERPROFILE')) = 0) or
    (CompareText(P, ExpandConstant('{localappdata}')) = 0);
  if IsWin64 then
    Result := Result or StartsWithPath(P, ExpandConstant('{commonpf64}'));
end;

procedure InitializeWizard;
var
  Page: TOutputMsgMemoWizardPage;
begin
  Page := CreateOutputMsgMemoPage(wpLicense,
    '启动器联网与本地数据', '请确认以下运行行为。',
    '安装器使用本地文本，不会加载在线协议网页。',
    '启动器按部署配置准备 Python、Node.js、Git 和依赖，并从配置的 Git 仓库更新后端。' + #13#10#13#10 +
    '启动器自身更新仅在构建时配置了更新地址和发行公钥后启用，更新须通过签名与文件摘要校验。' + #13#10#13#10 +
    '日志保存在本地 log 目录。导出文件须由你在系统保存对话框中选择位置。' + #13#10#13#10 +
    '安装和卸载会保留已有配置与运行数据；需要清除时，请退出程序后自行删除安装目录。');
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = wpSelectDir) and IsUnsafeInstallPath(WizardDirValue) then
  begin
    MsgBox('请选择当前用户可写的应用子目录，例如默认的 LocalAppData\Programs\AzurPilot。不能使用系统目录或用户目录根目录。', mbError, MB_OK);
    Result := False;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if IsUnsafeInstallPath(ExpandConstant('{app}')) then
    Result := '安装目录不安全，请使用默认的当前用户应用目录。';
end;
