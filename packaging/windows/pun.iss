; Per-user installer for pun.exe. No administrator rights.
; Build: ISCC /DPunExe=path\to\pun.exe /DPunVersion=0.4.3 pun.iss

#ifndef PunExe
  #define PunExe "..\..\target\dist\pun.exe"
#endif
#ifndef PunVersion
  #define PunVersion "0.4.3"
#endif

[Setup]
AppId={{8F3C1A2E-4B67-4D90-9E15-7C2A6B0D4E18}
AppName=Pun
AppVersion={#PunVersion}
AppPublisher=Pun
AppPublisherURL=https://github.com/GilbertrdzDev/pun
AppSupportURL=https://github.com/GilbertrdzDev/pun
DefaultDirName={localappdata}\Programs\pun
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\..\target\windows
OutputBaseFilename=pun-setup-x86_64
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\pun.exe
ChangesEnvironment=yes
CloseApplications=no

[Files]
Source: "{#PunExe}"; DestDir: "{app}"; DestName: "pun.exe"; Flags: ignoreversion

[UninstallDelete]
Type: files; Name: "{app}\pun.exe"

[Code]
const
  EnvironmentKey = 'Environment';
  WM_SETTINGCHANGE = $001A;
  HWND_BROADCAST = $FFFF;
  SMTO_ABORTIFHUNG = $0002;

function UserPath(var Path: string): Boolean;
begin
  Result := RegQueryStringValue(HKCU, EnvironmentKey, 'Path', Path);
  if not Result then
    Path := '';
  Result := True;
end;

function SameDir(const A, B: string): Boolean;
begin
  Result := CompareText(RemoveBackslash(A), RemoveBackslash(B)) = 0;
end;

function PathHasDir(const Path, Dir: string): Boolean;
var
  Piece, Rest: string;
  P: Integer;
begin
  Result := False;
  Rest := Path;
  while Rest <> '' do
  begin
    P := Pos(';', Rest);
    if P = 0 then
    begin
      Piece := Rest;
      Rest := '';
    end
    else
    begin
      Piece := Copy(Rest, 1, P - 1);
      Rest := Copy(Rest, P + 1, MaxInt);
    end;
    if SameDir(Piece, Dir) then
    begin
      Result := True;
      Exit;
    end;
  end;
end;

function PathWithoutDir(const Path, Dir: string): string;
var
  Piece, Rest, Next: string;
  P: Integer;
begin
  Result := '';
  Rest := Path;
  while Rest <> '' do
  begin
    P := Pos(';', Rest);
    if P = 0 then
    begin
      Piece := Rest;
      Rest := '';
    end
    else
    begin
      Piece := Copy(Rest, 1, P - 1);
      Rest := Copy(Rest, P + 1, MaxInt);
    end;
    if (Piece <> '') and (not SameDir(Piece, Dir)) then
    begin
      if Result = '' then
        Next := Piece
      else
        Next := Result + ';' + Piece;
      Result := Next;
    end;
  end;
end;

function SendMessageTimeout(hWnd: Integer; Msg: Integer; wParam: Integer; lParam: String; fuFlags: Integer; uTimeout: Integer; var lpdwResult: Integer): Integer;
  external 'SendMessageTimeoutW@user32.dll stdcall';

procedure BroadcastPathChange;
var
  Ignored: Integer;
begin
  SendMessageTimeout(HWND_BROADCAST, WM_SETTINGCHANGE, 0, 'Environment', SMTO_ABORTIFHUNG, 5000, Ignored);
end;

procedure WriteUserPath(const Path: string);
begin
  if not RegWriteStringValue(HKCU, EnvironmentKey, 'Path', Path) then
    RaiseException('Could not update the user PATH');
  BroadcastPathChange;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Path, Dir: string;
begin
  if CurStep <> ssPostInstall then
    Exit;
  Dir := ExpandConstant('{app}');
  UserPath(Path);
  if not PathHasDir(Path, Dir) then
  begin
    if Path = '' then
      Path := Dir
    else
      Path := Path + ';' + Dir;
    WriteUserPath(Path);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Path, Dir: string;
begin
  if CurUninstallStep <> usPostUninstall then
    Exit;
  Dir := ExpandConstant('{app}');
  UserPath(Path);
  if PathHasDir(Path, Dir) then
    WriteUserPath(PathWithoutDir(Path, Dir));
end;

function InitializeSetup(): Boolean;
var
  Code: Integer;
begin
  Result := True;
  if (not Exec('php', '-v', '', SW_HIDE, ewWaitUntilTerminated, Code)) or (Code <> 0) then
    MsgBox('PHP was not found on PATH.' + #13#10 + #13#10 +
      'Pun needs PHP 8.4 or later on PATH for install, update, require, and Composer scripts. ' +
      'Format, lint, and analyse work without PHP. You can install Pun now and add PHP afterwards.',
      mbInformation, MB_OK);
end;
