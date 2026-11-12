#ifndef AppVersion
  #error AppVersion must be supplied
#endif
; One installer for every Windows PC: it carries the ARM64 and the 32-bit x86 builds and
; installs the one that suits the processor (x86 also covers 64-bit Intel/AMD PCs).
#ifndef BinaryArm64
  #error BinaryArm64 (the aarch64-pc-windows-msvc see_my_mac.exe) must be supplied
#endif
#ifndef BinaryX86
  #error BinaryX86 (the i686-pc-windows-msvc see_my_mac.exe) must be supplied
#endif
#ifndef BuildSuffix
  #define BuildSuffix ""
#endif

[Setup]
AppId={{D1F1A5A8-5F16-4F62-A8FA-5EBACDC8A140}
AppName=SeeMyMac
AppVersion={#AppVersion}
DefaultDirName={autopf}\SeeMyMac
DefaultGroupName=SeeMyMac
PrivilegesRequired=lowest
OutputDir=..\..\target\installers
OutputBaseFilename=SeeMyMac-{#AppVersion}{#BuildSuffix}-setup
SetupIconFile=..\..\assets\icons\seemac.ico
UninstallDisplayIcon={app}\see_my_mac.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; x86compatible: any PC that runs x86 programs (x86, x64 and ARM64 Windows).
ArchitecturesAllowed=x86compatible
ArchitecturesInstallIn64BitMode=arm64

[Files]
Source: "{#BinaryArm64}"; DestDir: "{app}"; DestName: "see_my_mac.exe"; Check: IsArm64; Flags: ignoreversion
Source: "{#BinaryX86}"; DestDir: "{app}"; DestName: "see_my_mac.exe"; Check: not IsArm64; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\SeeMyMac"; Filename: "{app}\see_my_mac.exe"

[Run]
Filename: "{app}\see_my_mac.exe"; Description: "Launch SeeMyMac"; Flags: nowait postinstall skipifsilent
Filename: "{app}\see_my_mac.exe"; Flags: nowait; Check: ShouldRelaunch

[Code]
// The in-app updater runs this installer as: /VERYSILENT /relaunch=1 /waitpid=<its process id>.
// Setup waits for that process to exit (the program file is locked while it runs), installs over
// the existing copy in the user's profile (PrivilegesRequired=lowest, so no elevation prompt),
// and starts SeeMyMac again.
function OpenProcess(dwDesiredAccess: DWORD; bInheritHandle: BOOL; dwProcessId: DWORD): THandle;
  external 'OpenProcess@kernel32.dll stdcall';
function WaitForSingleObject(hHandle: THandle; dwMilliseconds: DWORD): DWORD;
  external 'WaitForSingleObject@kernel32.dll stdcall';
function CloseHandle(hObject: THandle): BOOL;
  external 'CloseHandle@kernel32.dll stdcall';

function InitializeSetup(): Boolean;
var
  Pid: Integer;
  Handle: THandle;
begin
  Result := True;
  Pid := StrToIntDef(ExpandConstant('{param:waitpid|0}'), 0);
  if Pid > 0 then
  begin
    Handle := OpenProcess($00100000, False, Pid); // SYNCHRONIZE
    if Handle <> 0 then
    begin
      WaitForSingleObject(Handle, 30000);
      CloseHandle(Handle);
    end;
  end;
end;

function ShouldRelaunch(): Boolean;
begin
  Result := ExpandConstant('{param:relaunch|0}') = '1';
end;
