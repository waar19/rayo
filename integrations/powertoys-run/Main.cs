using System.Diagnostics;
using System.IO;
using System.Windows;
using System.Windows.Input;
using Wox.Plugin;

namespace Community.PowerToys.Run.Plugin.Rayo;

public sealed class Main : IPlugin, IDelayedExecutionPlugin, IContextMenu
{
    public static string PluginID => "F8074EA2F6CF4A3A8D996BBA5F95F185";
    private const string BackgroundTaskName = "Rayo Service";
    private const string ServiceExecutableName = "rayo-service.exe";
    private const string ServicePathEnvironmentVariable = "RAYO_SERVICE_PATH";

    public string Name => "Rayo";

    public string Description => "Fast file search powered by Rayo service.";

    private readonly RayoPipeClient _pipeClient = new();
    private PluginInitContext? _pluginContext;

    public void Init(PluginInitContext context)
    {
        _pluginContext = context;
    }

    public List<Result> Query(Query query)
    {
        return Query(query, delayedExecution: false);
    }

    public List<Result> Query(Query query, bool delayedExecution)
    {
        var input = query?.Search?.Trim() ?? string.Empty;
        if (string.IsNullOrWhiteSpace(input))
        {
            return [];
        }

        try
        {
            var response = _pipeClient
                .QueryAsync(input, limit: 20)
                .GetAwaiter()
                .GetResult();
            if (response?.Results is null || response.Results.Count == 0)
            {
                return [];
            }

            var mapped = new List<Result>(response.Results.Count);
            for (var idx = 0; idx < response.Results.Count; idx++)
            {
                var item = response.Results[idx];
                var title = Path.GetFileName(item.Path);
                if (string.IsNullOrWhiteSpace(title))
                {
                    title = item.Path;
                }

                mapped.Add(new Result
                {
                    Title = title,
                    SubTitle = item.Path,
                    Score = 10_000 - idx,
                    IcoPath = item.IsDirectory ? "Images\\rayo.folder.png" : "Images\\rayo.file.png",
                    ContextData = item,
                    Action = action =>
                    {
                        if (action?.SpecialKeyState?.CtrlPressed == true)
                        {
                            return OpenContainingFolder(item.Path);
                        }
                        return OpenItem(item.Path, asAdmin: false);
                    },
                });
            }

            return mapped;
        }
        catch (Exception)
        {
            return
            [
                new Result
                {
                    Title = "Rayo service not running",
                    SubTitle = "Press Enter to start rayo-service as administrator.",
                    Score = int.MaxValue,
                    IcoPath = "Images\\rayo.warn.png",
                    Action = _ => TryStartService(),
                },
            ];
        }
    }

    public List<ContextMenuResult> LoadContextMenus(Result selectedResult)
    {
        if (selectedResult?.ContextData is not QueryResultItem item)
        {
            return [];
        }

        return
        [
            new ContextMenuResult
            {
                PluginName = Name,
                Title = "Open as administrator",
                Glyph = "\uE7EF",
                FontFamily = "Segoe Fluent Icons",
                AcceleratorKey = Key.Enter,
                AcceleratorModifiers = ModifierKeys.Control | ModifierKeys.Shift,
                Action = _ => OpenItem(item.Path, asAdmin: true),
            },
            new ContextMenuResult
            {
                PluginName = Name,
                Title = "Open containing folder",
                Glyph = "\uE838",
                FontFamily = "Segoe Fluent Icons",
                AcceleratorKey = Key.E,
                AcceleratorModifiers = ModifierKeys.Control | ModifierKeys.Shift,
                Action = _ => OpenContainingFolder(item.Path),
            },
            new ContextMenuResult
            {
                PluginName = Name,
                Title = "Copy path",
                Glyph = "\uE8C8",
                FontFamily = "Segoe Fluent Icons",
                AcceleratorKey = Key.C,
                AcceleratorModifiers = ModifierKeys.Control | ModifierKeys.Shift,
                Action = _ => CopyPath(item.Path),
            },
        ];
    }

    private static bool OpenItem(string path, bool asAdmin)
    {
        try
        {
            var start = new ProcessStartInfo
            {
                FileName = path,
                UseShellExecute = true,
            };
            if (asAdmin)
            {
                start.Verb = "runas";
            }

            Process.Start(start);
            return true;
        }
        catch
        {
            return false;
        }
    }

    private static bool OpenContainingFolder(string path)
    {
        try
        {
            if (Directory.Exists(path))
            {
                Process.Start(
                    new ProcessStartInfo
                    {
                        FileName = path,
                        UseShellExecute = true,
                    }
                );
                return true;
            }

            Process.Start(
                new ProcessStartInfo
                {
                    FileName = "explorer.exe",
                    Arguments = $"/select,\"{path}\"",
                    UseShellExecute = true,
                }
            );
            return true;
        }
        catch
        {
            return false;
        }
    }

    private static bool CopyPath(string path)
    {
        try
        {
            Clipboard.SetText(path);
            return false;
        }
        catch
        {
            return false;
        }
    }

    private bool TryStartService()
    {
        if (TryStartBackgroundTask())
        {
            ShowStatus("Starting Rayo background task as administrator. Retry search in a few seconds.");
            return false;
        }

        var servicePath = ResolveServicePath();
        if (servicePath is null)
        {
            ShowStatus("Rayo Service task not found and rayo-service.exe was not found. Reinstall plugin or set RAYO_SERVICE_PATH.");
            return false;
        }

        try
        {
            Process.Start(
                new ProcessStartInfo
                {
                    FileName = servicePath,
                    UseShellExecute = true,
                    Verb = "runas",
                }
            );
            ShowStatus("Starting rayo-service as administrator. Retry search in a few seconds.");
            return false;
        }
        catch
        {
            ShowStatus("Could not start rayo-service. Confirm UAC and try again.");
            return false;
        }
    }

    private static bool TryStartBackgroundTask()
    {
        try
        {
            using var process = Process.Start(
                new ProcessStartInfo
                {
                    FileName = "schtasks.exe",
                    Arguments = $"/run /tn \"{BackgroundTaskName}\"",
                    UseShellExecute = true,
                    Verb = "runas",
                    WindowStyle = ProcessWindowStyle.Hidden,
                }
            );
            if (process is null)
            {
                return false;
            }

            process.WaitForExit(8000);
            return process.ExitCode == 0;
        }
        catch
        {
            return false;
        }
    }

    private string? ResolveServicePath()
    {
        var configuredPath = Environment.GetEnvironmentVariable(ServicePathEnvironmentVariable);
        var configuredCandidate = NormalizePathCandidate(configuredPath);
        if (configuredCandidate is not null && File.Exists(configuredCandidate))
        {
            return configuredCandidate;
        }

        var localAppData = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
        if (!string.IsNullOrWhiteSpace(localAppData))
        {
            var defaultPath = Path.Combine(localAppData, "Rayo", ServiceExecutableName);
            if (File.Exists(defaultPath))
            {
                return defaultPath;
            }
        }

        var pathValue = Environment.GetEnvironmentVariable("PATH");
        if (string.IsNullOrWhiteSpace(pathValue))
        {
            return null;
        }

        var pathEntries = pathValue.Split(Path.PathSeparator, StringSplitOptions.RemoveEmptyEntries);
        foreach (var entry in pathEntries)
        {
            var normalizedEntry = NormalizePathCandidate(entry);
            if (normalizedEntry is null)
            {
                continue;
            }

            var candidate = Path.Combine(normalizedEntry, ServiceExecutableName);
            if (File.Exists(candidate))
            {
                return candidate;
            }
        }

        return null;
    }

    private void ShowStatus(string message)
    {
        try
        {
            _pluginContext?.API?.ShowMsg(Name, message);
        }
        catch
        {
            // Ignore UI notification errors to avoid blocking actions.
        }
    }

    private static string? NormalizePathCandidate(string? rawPath)
    {
        if (string.IsNullOrWhiteSpace(rawPath))
        {
            return null;
        }

        var expanded = Environment.ExpandEnvironmentVariables(rawPath.Trim());
        var withoutQuotes = expanded.Trim('"');
        return string.IsNullOrWhiteSpace(withoutQuotes) ? null : withoutQuotes;
    }
}
