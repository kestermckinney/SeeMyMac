#ifndef AppVersion
  #error AppVersion must be supplied
#endif
#ifndef AppArch
  #error AppArch must be supplied
#endif
#ifndef AppBinary
  #error AppBinary must be supplied
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
OutputBaseFilename=SeeMyMac-{#AppVersion}-{#AppArch}{#BuildSuffix}-setup
SetupIconFile=..\..\assets\icons\seemac.ico
UninstallDisplayIcon={app}\see_my_mac.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
#if AppArch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#endif

[Files]
Source: "{#AppBinary}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\SeeMyMac"; Filename: "{app}\see_my_mac.exe"

[Run]
Filename: "{app}\see_my_mac.exe"; Description: "Launch SeeMyMac"; Flags: nowait postinstall skipifsilent
