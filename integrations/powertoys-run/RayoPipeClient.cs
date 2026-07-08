using System.IO.Pipes;
using System.Text;
using System.Text.Json;

namespace Community.PowerToys.Run.Plugin.Rayo;

public sealed class RayoPipeClient
{
    private readonly string _pipeName;

    public RayoPipeClient(string pipeName = "rayo-query")
    {
        _pipeName = pipeName;
    }

    public async Task<QueryResponse?> QueryAsync(string query, CancellationToken cancellationToken = default)
    {
        using var client = new NamedPipeClientStream(
            ".",
            _pipeName,
            PipeDirection.InOut,
            PipeOptions.Asynchronous
        );

        await client.ConnectAsync(300, cancellationToken).ConfigureAwait(false);
        await using var writer = new StreamWriter(client, Encoding.UTF8, leaveOpen: true);
        using var reader = new StreamReader(client, Encoding.UTF8, leaveOpen: true);

        var request = new QueryRequest
        {
            Query = query,
            Limit = 10,
            DirectoriesOnly = false,
            FilesOnly = false
        };

        var json = JsonSerializer.Serialize(request);
        await writer.WriteLineAsync(json).ConfigureAwait(false);
        await writer.FlushAsync().ConfigureAwait(false);

        var responseLine = await reader.ReadLineAsync(cancellationToken).ConfigureAwait(false);
        if (string.IsNullOrWhiteSpace(responseLine))
        {
            return null;
        }

        return JsonSerializer.Deserialize<QueryResponse>(responseLine);
    }

    private sealed class QueryRequest
    {
        public string Query { get; set; } = string.Empty;
        public string? Extension { get; set; }
        public string? UnderDir { get; set; }
        public string? Glob { get; set; }
        public bool DirectoriesOnly { get; set; }
        public bool FilesOnly { get; set; }
        public int? Limit { get; set; }
    }
}

public sealed class QueryResponse
{
    public ulong TookMs { get; set; }
    public int TotalEntries { get; set; }
    public List<QueryResultItem> Results { get; set; } = new();
}

public sealed class QueryResultItem
{
    public string Path { get; set; } = string.Empty;
    public bool IsDirectory { get; set; }
}
