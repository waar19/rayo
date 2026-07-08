# PowerToys Run Plugin (Rayo)

This folder contains a functional PowerToys Run plugin project:
`Community.PowerToys.Run.Plugin.Rayo`.

Current scope:
- `Main.cs` implements `IPlugin`, `IDelayedExecutionPlugin`, and `IContextMenu`.
- Queries `\\.\pipe\rayo-query` and maps results to launcher entries.
- Supports actions: open, open containing folder, open as administrator, copy path.
- Includes `plugin.json` and plugin icons for packaging.

Build:

```powershell
dotnet build .\integrations\powertoys-run\Community.PowerToys.Run.Plugin.Rayo.csproj -c Release
```

Manual installation (PowerToys Run):
1. Build plugin in Release.
2. Copy output folder contents to:
   `%LOCALAPPDATA%\Microsoft\PowerToys\PowerToys Run\Plugins\Rayo\`
3. Restart PowerToys.
4. Use action keyword `ry` in PowerToys Run.

Automatic installer with dependency detection:

```powershell
pwsh .\scripts\install-powertoys-plugin.ps1 -PluginZipPath .\dist\powertoys-run\RayoPlugin.zip -AutoInstallDependencies -RestartPowerToys
```
