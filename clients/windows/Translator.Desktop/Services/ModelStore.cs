using System.Net.Http;
using System.Text.Json;

namespace Translator.Desktop.Services;

public sealed record WhisperModel(string Id, string Description)
{
    public string FileName => $"ggml-{(Id == "large" ? "large-v3" : Id)}.bin";
    public bool EnglishOnly => Id.EndsWith(".en", StringComparison.Ordinal);
    public override string ToString() => $"{Id} · {Description}";
}

public sealed record RecognitionSettings(string Provider = "whisper", string Engine = "cuda", string Model = "tiny", string Directory = "", string Language = "");

public static class ModelStore
{
    public static readonly WhisperModel[] Models =
    [
        new("tiny", "多语言 · 约 75 MiB"), new("tiny.en", "仅英语 · 约 75 MiB"),
        new("base", "多语言 · 约 142 MiB"), new("base.en", "仅英语 · 约 142 MiB"),
        new("small", "多语言 · 约 466 MiB"), new("small.en", "仅英语 · 约 466 MiB"),
        new("medium", "多语言 · 约 1.5 GiB"), new("medium.en", "仅英语 · 约 1.5 GiB"),
        new("large", "最新版 large-v3 · 约 2.9 GiB"), new("large-v1", "多语言 · 约 2.9 GiB"),
        new("large-v2", "多语言 · 约 2.9 GiB"), new("large-v3", "多语言 · 约 2.9 GiB")
    ];
    private static readonly string AppDirectory = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Translator");
    public static string DefaultDirectory => Path.Combine(AppDirectory, "models");
    private static readonly string SettingsFile = Path.Combine(AppDirectory, "recognition.json");
    private static readonly HttpClient Http = new() { Timeout = Timeout.InfiniteTimeSpan };

    public static RecognitionSettings Load()
    {
        try { return JsonSerializer.Deserialize<RecognitionSettings>(File.ReadAllText(SettingsFile)) ?? new(); }
        catch { return new(); }
    }
    public static void Save(RecognitionSettings settings)
    {
        Directory.CreateDirectory(AppDirectory);
        File.WriteAllText(SettingsFile + ".tmp", JsonSerializer.Serialize(settings));
        File.Move(SettingsFile + ".tmp", SettingsFile, true);
    }
    public static bool IsReady(string path)
    {
        try
        {
            using var file = File.OpenRead(path);
            Span<byte> magic = stackalloc byte[4];
            return file.Length > 4 && file.Read(magic) == 4 && magic.SequenceEqual("lmgg"u8);
        }
        catch { return false; }
    }

    public static async Task EnsureAsync(WhisperModel model, string folder, IProgress<(long Bytes, long? Total)> progress, CancellationToken token)
    {
        if (!Path.IsPathFullyQualified(folder)) throw new IOException("请选择绝对路径的模型文件夹。");
        Directory.CreateDirectory(folder);
        var path = Path.Combine(folder, model.FileName);
        if (IsReady(path)) return;
        // Use a unique temporary file; only complete validated downloads become models.
        var temporary = path + $".{Guid.NewGuid():N}.part";
        try
        {
            var baseUrl = Environment.GetEnvironmentVariable("TRANSLATOR_MODEL_BASE_URL")
                ?? "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/";
            using var requestTimeout = CancellationTokenSource.CreateLinkedTokenSource(token);
            requestTimeout.CancelAfter(TimeSpan.FromSeconds(60));
            using var response = await Http.GetAsync(baseUrl.TrimEnd('/') + "/" + model.FileName, HttpCompletionOption.ResponseHeadersRead, requestTimeout.Token);
            response.EnsureSuccessStatusCode();
            var total = response.Content.Headers.ContentLength;
            await using var input = await response.Content.ReadAsStreamAsync(token);
            long received = 0;
            await using (var output = new FileStream(temporary, FileMode.CreateNew, FileAccess.Write, FileShare.None, 131072, true))
            {
                var buffer = new byte[131072];
                while (true)
                {
                    using var readTimeout = CancellationTokenSource.CreateLinkedTokenSource(token);
                    readTimeout.CancelAfter(TimeSpan.FromSeconds(60));
                    var count = await input.ReadAsync(buffer, readTimeout.Token);
                    if (count == 0) break;
                    await output.WriteAsync(buffer.AsMemory(0, count), token);
                    received += count;
                    progress.Report((received, total));
                }
                await output.FlushAsync(token);
            }
            token.ThrowIfCancellationRequested();
            if (total.HasValue && total.Value != received) throw new IOException("模型下载不完整，请重试。");
            if (!IsReady(temporary)) throw new IOException("下载内容不是有效的 Whisper GGML 模型，请检查下载源。");
            File.Move(temporary, path, true);
        }
        finally { if (File.Exists(temporary)) File.Delete(temporary); }
    }
}
