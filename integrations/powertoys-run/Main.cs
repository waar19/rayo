using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Net.Http;
using System.Text.Json;
using System.Windows;
using System.Windows.Input;
using Wox.Plugin;

namespace Community.PowerToys.Run.Plugin.Rayo;

public sealed class Main : IPlugin, IDelayedExecutionPlugin, IContextMenu
{
    public static string PluginID => "F8074EA2F6CF4A3A8D996BBA5F95F185";
    private const string BackgroundTaskName = "Rayo Service";
    private const string ServiceProcessName = "rayo-service";
    private const string ServiceExecutableName = "rayo-service.exe";
    private const string ServicePathEnvironmentVariable = "RAYO_SERVICE_PATH";
    private const string PluginVersion = "0.4.1";
    private const string ReleasesLatestApi = "https://api.github.com/repos/waar19/rayo/releases/latest";
    private static readonly HttpClient UpdateHttpClient = new() { Timeout = TimeSpan.FromSeconds(4) };
    private static readonly TimeSpan AppCatalogTtl = TimeSpan.FromMinutes(10);

    public string Name => "Rayo";

    public string Description => "Fast file search powered by Rayo service.";

    private readonly RayoPipeClient _pipeClient = new();
    private PluginInitContext? _pluginContext;
    private bool _updateCheckStarted;
    private readonly object _appCatalogLock = new();
    private List<AppEntry> _appCatalog = [];
    private DateTimeOffset _appCatalogBuiltAt = DateTimeOffset.MinValue;

    public void Init(PluginInitContext context)
    {
        _pluginContext = context;
        StartUpdateCheck();
    }

    public List<Result> Query(Query query)
    {
        return Query(query, delayedExecution: false);
    }

    public List<Result> Query(Query query, bool delayedExecution)
    {
        var isGlobalQuery = string.IsNullOrWhiteSpace(query?.ActionKeyword);
        var input = query?.Search?.Trim() ?? string.Empty;
        var mode = "name";
        if (!isGlobalQuery && input.StartsWith("c ", StringComparison.OrdinalIgnoreCase))
        {
            mode = "content";
            input = input[2..].Trim();
        }
        if (string.IsNullOrWhiteSpace(input))
        {
            return [];
        }
        if (isGlobalQuery)
        {
            if (!delayedExecution)
            {
                return [];
            }
            if (input.Length < 2)
            {
                return [];
            }
        }

        try
        {
            var response = _pipeClient
                .QueryAsync(input, limit: 20, mode: mode, timeoutMs: mode == "content" ? 3_000 : null)
                .GetAwaiter()
                .GetResult();
            if (response is null)
            {
                return [];
            }
            if (string.Equals(response.Status, "starting", StringComparison.OrdinalIgnoreCase))
            {
                return isGlobalQuery ? [] : [BuildServiceStartingResult(response.IndexedEntries)];
            }
            if (response?.Results is null || response.Results.Count == 0)
            {
                return [];
            }

            var mapped = new List<Result>(response.Results.Count);
            var scoreBase = isGlobalQuery ? 100 : 10_000;
            var seenPaths = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            for (var idx = 0; idx < response.Results.Count; idx++)
            {
                var item = response.Results[idx];
                seenPaths.Add(item.Path);
                var title = Path.GetFileName(item.Path);
                if (string.IsNullOrWhiteSpace(title))
                {
                    title = item.Path;
                }

                mapped.Add(new Result
                {
                    Title = title,
                    SubTitle = FormatSubTitle(item),
                    Score = scoreBase - idx,
                    IcoPath = ResolveItemIconPath(item),
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

            if (!string.Equals(mode, "content", StringComparison.OrdinalIgnoreCase))
            {
                mapped.AddRange(BuildAppResults(input, isGlobalQuery, mapped.Count, seenPaths));
                if (mapped.Count > 20)
                {
                    mapped = mapped.Take(20).ToList();
                }
            }

            return mapped;
        }
        catch (TimeoutException) when (IsServiceProcessRunning())
        {
            return isGlobalQuery ? [] : [BuildServiceStartingResult(indexedEntries: null)];
        }
        catch (Exception)
        {
            if (IsServiceProcessRunning())
            {
                return isGlobalQuery ? [] : [BuildServiceStartingResult(indexedEntries: null)];
            }
            return isGlobalQuery ? [] : [BuildServiceNotRunningResult()];
        }
    }

    private static string FormatSubTitle(QueryResultItem item)
    {
        if (item.LineNumber is > 0)
        {
            var excerpt = string.IsNullOrWhiteSpace(item.LineText) ? string.Empty : $"  {item.LineText.Trim()}";
            return $"{item.Path}:{item.LineNumber}{excerpt}";
        }
        return item.Path;
    }

    private static string ResolveItemIconPath(QueryResultItem item)
    {
        if (item.IsDirectory)
        {
            return "Images\\rayo.folder.png";
        }

        try
        {
            if (!string.IsNullOrWhiteSpace(item.Path) && File.Exists(item.Path))
            {
                // PowerToys extracts associated icon from file/shortcut/exe path.
                return item.Path;
            }
        }
        catch
        {
            // Ignore icon resolution errors and fall back to static icon.
        }
        return "Images\\rayo.file.png";
    }

    private List<Result> BuildAppResults(
        string input,
        bool isGlobalQuery,
        int existingCount,
        HashSet<string> seenPaths
    )
    {
        if (string.IsNullOrWhiteSpace(input) || input.Length < 2)
        {
            return [];
        }

        var appLimit = Math.Clamp(20 - existingCount, 0, 8);
        if (appLimit == 0)
        {
            return [];
        }

        var apps = SearchApps(input, appLimit, seenPaths);
        if (apps.Count == 0)
        {
            return [];
        }

        var scoreBase = isGlobalQuery ? 90 : 9_000;
        var mapped = new List<Result>(apps.Count);
        for (var idx = 0; idx < apps.Count; idx++)
        {
            var app = apps[idx];
            var iconPath = File.Exists(app.LaunchPath) ? app.LaunchPath : "Images\\rayo.file.png";
            var contextItem = new QueryResultItem
            {
                Path = app.LaunchPath,
                IsDirectory = false,
            };
            mapped.Add(
                new Result
                {
                    Title = app.DisplayName,
                    SubTitle = $"App \u2022 {app.LaunchPath}",
                    Score = scoreBase - idx,
                    IcoPath = iconPath,
                    ContextData = contextItem,
                    Action = action =>
                    {
                        if (action?.SpecialKeyState?.CtrlPressed == true)
                        {
                            return OpenContainingFolder(app.LaunchPath);
                        }
                        return OpenItem(app.LaunchPath, asAdmin: false);
                    },
                }
            );
        }
        return mapped;
    }

    private List<AppEntry> SearchApps(string query, int limit, HashSet<string> seenPaths)
    {
        var catalog = GetOrBuildAppCatalog();
        return catalog
            .Where(app => !seenPaths.Contains(app.LaunchPath))
            .Where(app =>
                app.DisplayName.Contains(query, StringComparison.OrdinalIgnoreCase)
                || app.LaunchPath.Contains(query, StringComparison.OrdinalIgnoreCase)
            )
            .OrderBy(app => app.DisplayName.StartsWith(query, StringComparison.OrdinalIgnoreCase) ? 0 : 1)
            .ThenBy(app => app.DisplayName.Length)
            .Take(limit)
            .ToList();
    }

    private List<AppEntry> GetOrBuildAppCatalog()
    {
        lock (_appCatalogLock)
        {
            var now = DateTimeOffset.UtcNow;
            if (_appCatalog.Count > 0 && (now - _appCatalogBuiltAt) < AppCatalogTtl)
            {
                return _appCatalog;
            }

            _appCatalog = BuildAppCatalog();
            _appCatalogBuiltAt = now;
            return _appCatalog;
        }
    }

    private static List<AppEntry> BuildAppCatalog()
    {
        var roots = new List<string>();
        AddIfDirectoryExists(roots, Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.StartMenu), "Programs"));
        AddIfDirectoryExists(
            roots,
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.CommonStartMenu), "Programs")
        );
        AddIfDirectoryExists(
            roots,
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Microsoft", "WindowsApps")
        );

        var entries = new Dictionary<string, AppEntry>(StringComparer.OrdinalIgnoreCase);
        var allowedExtensions = new HashSet<string>(StringComparer.OrdinalIgnoreCase) { ".lnk", ".appref-ms", ".exe" };

        foreach (var root in roots.Distinct(StringComparer.OrdinalIgnoreCase))
        {
            IEnumerable<string> files;
            try
            {
                files = Directory.EnumerateFiles(root, "*", SearchOption.AllDirectories);
            }
            catch
            {
                continue;
            }

            foreach (var file in files)
            {
                var extension = Path.GetExtension(file);
                if (!allowedExtensions.Contains(extension))
                {
                    continue;
                }

                var name = Path.GetFileNameWithoutExtension(file)?.Trim();
                if (string.IsNullOrWhiteSpace(name))
                {
                    continue;
                }
                if (name.StartsWith("Uninstall", StringComparison.OrdinalIgnoreCase))
                {
                    continue;
                }

                if (!entries.ContainsKey(file))
                {
                    entries[file] = new AppEntry(name, file);
                }
            }
        }

        return entries.Values.OrderBy(item => item.DisplayName, StringComparer.OrdinalIgnoreCase).ToList();
    }

    private static void AddIfDirectoryExists(List<string> roots, string? path)
    {
        if (!string.IsNullOrWhiteSpace(path) && Directory.Exists(path))
        {
            roots.Add(path);
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

    private Result BuildServiceNotRunningResult()
    {
        return new Result
        {
            Title = "Rayo service not running",
            SubTitle = "Press Enter to start rayo-service as administrator.",
            Score = int.MaxValue,
            IcoPath = "Images\\rayo.warn.png",
            Action = _ => TryStartService(),
        };
    }

    private static Result BuildServiceStartingResult(int? indexedEntries)
    {
        var subtitle = indexedEntries is > 0
            ? $"Rayo is indexing in the background ({indexedEntries:N0} entries scanned). Retry in a few seconds."
            : "Rayo is starting in the background. Retry in a few seconds.";
        return new Result
        {
            Title = "Rayo is starting",
            SubTitle = subtitle,
            Score = int.MaxValue,
            IcoPath = "Images\\rayo.warn.png",
            Action = _ => false,
        };
    }

    private static bool IsServiceProcessRunning()
    {
        try
        {
            return Process.GetProcessesByName(ServiceProcessName).Length > 0;
        }
        catch
        {
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

    private void StartUpdateCheck()
    {
        if (_updateCheckStarted)
        {
            return;
        }
        _updateCheckStarted = true;
        _ = Task.Run(CheckForUpdatesAsync);
    }

    private async Task CheckForUpdatesAsync()
    {
        try
        {
            var statePath = Path.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
                "Rayo",
                "plugin-update-check.json"
            );
            var stateDir = Path.GetDirectoryName(statePath);
            if (!string.IsNullOrWhiteSpace(stateDir))
            {
                Directory.CreateDirectory(stateDir);
            }

            var now = DateTimeOffset.UtcNow;
            var state = await LoadUpdateStateAsync(statePath).ConfigureAwait(false);
            if (state.CheckedAt > now.AddHours(-24) && !string.IsNullOrWhiteSpace(state.LatestVersion))
            {
                if (IsVersionNewer(state.LatestVersion!, PluginVersion))
                {
                    ShowStatus($"New Rayo version available ({state.LatestVersion}). Run installer to update.");
                }
                return;
            }

            using var request = new HttpRequestMessage(HttpMethod.Get, ReleasesLatestApi);
            request.Headers.TryAddWithoutValidation("User-Agent", "rayo-powertoys-plugin");
            using var response = await UpdateHttpClient.SendAsync(request).ConfigureAwait(false);
            if (!response.IsSuccessStatusCode)
            {
                return;
            }
            var body = await response.Content.ReadAsStringAsync().ConfigureAwait(false);
            using var json = JsonDocument.Parse(body);
            if (!json.RootElement.TryGetProperty("tag_name", out var tagProperty))
            {
                return;
            }

            var latest = (tagProperty.GetString() ?? string.Empty).Trim().TrimStart('v', 'V');
            if (string.IsNullOrWhiteSpace(latest))
            {
                return;
            }

            var nextState = new UpdateState { CheckedAt = now, LatestVersion = latest };
            await SaveUpdateStateAsync(statePath, nextState).ConfigureAwait(false);
            if (IsVersionNewer(latest, PluginVersion))
            {
                ShowStatus($"New Rayo version available ({latest}). Run installer to update.");
            }
        }
        catch
        {
            // Ignore update-check failures to avoid impacting searches.
        }
    }

    private static bool IsVersionNewer(string latest, string current)
    {
        static Version ParseVersion(string value)
        {
            return Version.TryParse(value.Trim().TrimStart('v', 'V'), out var parsed)
                ? parsed
                : new Version(0, 0);
        }

        return ParseVersion(latest) > ParseVersion(current);
    }

    private static async Task<UpdateState> LoadUpdateStateAsync(string path)
    {
        if (!File.Exists(path))
        {
            return new UpdateState { CheckedAt = DateTimeOffset.MinValue, LatestVersion = null };
        }

        try
        {
            var json = await File.ReadAllTextAsync(path).ConfigureAwait(false);
            return JsonSerializer.Deserialize<UpdateState>(json)
                ?? new UpdateState { CheckedAt = DateTimeOffset.MinValue, LatestVersion = null };
        }
        catch
        {
            return new UpdateState { CheckedAt = DateTimeOffset.MinValue, LatestVersion = null };
        }
    }

    private static Task SaveUpdateStateAsync(string path, UpdateState state)
    {
        var content = JsonSerializer.Serialize(state);
        return File.WriteAllTextAsync(path, content);
    }

    private sealed record AppEntry(string DisplayName, string LaunchPath);

    private sealed class UpdateState
    {
        public DateTimeOffset CheckedAt { get; set; }
        public string? LatestVersion { get; set; }
    }
}
