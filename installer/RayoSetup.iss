#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

#define MyAppName "Rayo"
#define MyAppPublisher "waar19"
#define MyAppURL "https://github.com/waar19/rayo"

[Setup]
AppId={{F6C887FA-0A52-44D8-8C8F-2C7A090E3240}
AppName={#MyAppName}
AppVersion={#AppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\Rayo
DefaultGroupName=Rayo
OutputDir=..\dist
OutputBaseFilename=RayoSetup
Compression=lzma
SolidCompression=yes
ArchitecturesAllowed=x64 arm64
ArchitecturesInstallIn64BitMode=x64 arm64
PrivilegesRequired=lowest
WizardStyle=modern
LicenseFile=..\LICENSE

[Files]
Source: "..\dist\rayo-windows.zip"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\powertoys-run\RayoPlugin.zip"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\scripts\install-powertoys-plugin.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\scripts\uninstall-powertoys-plugin.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.es.md"; DestDir: "{app}"; Flags: ignoreversion

[Run]
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\install-powertoys-plugin.ps1"" -PluginZipPath ""{app}\RayoPlugin.zip"" -WindowsBundleZipPath ""{app}\rayo-windows.zip"" -AutoInstallDependencies $true -RestartPowerToys $true"; Flags: runhidden waituntilterminated

[UninstallRun]
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\uninstall-powertoys-plugin.ps1"" -RestartPowerToys $true"; Flags: runhidden waituntilterminated
