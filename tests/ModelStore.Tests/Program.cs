using System.Net;
using System.Net.Sockets;
using Translator.Desktop.Services;

var folder = Path.Combine(Path.GetTempPath(), "translator-model-test-" + Guid.NewGuid().ToString("N"));
Directory.CreateDirectory(folder);
var originalSource = Environment.GetEnvironmentVariable("TRANSLATOR_MODEL_BASE_URL");
try
{
    Assert(ModelStore.Models.Single(m => m.Id == "large").FileName == "ggml-large-v3.bin", "large alias");
    Assert(ModelStore.Models.Single(m => m.Id == "tiny.en").EnglishOnly, "English model");
    var model = ModelStore.Models[0];
    var target = Path.Combine(folder, model.FileName);
    var payload = new byte[1024]; "lmgg"u8.CopyTo(payload);
    await Serve(payload, 200, async token => await ModelStore.EnsureAsync(model, folder, new NoProgress(), token));
    Assert(ModelStore.IsReady(target), "successful atomic download");
    Environment.SetEnvironmentVariable("TRANSLATOR_MODEL_BASE_URL", "http://127.0.0.1:1/");
    await ModelStore.EnsureAsync(model, folder, new NoProgress(), default);
    Assert(ModelStore.IsReady(target), "existing model reused without request");
    File.Delete(target);
    await Serve("not a model"u8.ToArray(), 200, async token =>
    {
        await ExpectFailure(() => ModelStore.EnsureAsync(model, folder, new NoProgress(), token));
        Assert(!File.Exists(target), "bad content is never promoted");
    });
    await Serve([], 404, async token => await ExpectFailure(() => ModelStore.EnsureAsync(model, folder, new NoProgress(), token)));
    await Serve(payload, 200, async token =>
    {
        using var cancel = new CancellationTokenSource(150);
        await ExpectFailure(() => ModelStore.EnsureAsync(model, folder, new NoProgress(), cancel.Token));
        Assert(!File.Exists(target), "cancellation is never promoted");
    }, stall: true);
    Assert(!Directory.EnumerateFiles(folder, "*.part").Any(), "partial files cleaned");
    Console.WriteLine("PASS: aliases, English-only flag, streamed download, cache reuse, invalid content, HTTP failure, cancellation, partial cleanup");
}
finally
{
    Environment.SetEnvironmentVariable("TRANSLATOR_MODEL_BASE_URL", originalSource);
    Directory.Delete(folder, recursive: true);
}
static void Assert(bool condition, string name) { if (!condition) throw new Exception(name); }
static async Task ExpectFailure(Func<Task> action)
{
    try { await action(); }
    catch (Exception ex) when (ex is IOException or HttpRequestException or OperationCanceledException) { return; }
    throw new Exception("Expected a download failure");
}
static async Task Serve(byte[] body, int status, Func<CancellationToken, Task> test, bool stall = false)
{
    var portProbe = new TcpListener(IPAddress.Loopback, 0); portProbe.Start();
    var port = ((IPEndPoint)portProbe.LocalEndpoint).Port; portProbe.Stop();
    using var listener = new HttpListener();
    var url = $"http://127.0.0.1:{port}/"; listener.Prefixes.Add(url); listener.Start();
    Environment.SetEnvironmentVariable("TRANSLATOR_MODEL_BASE_URL", url);
    var server = Task.Run(async () =>
    {
        var context = await listener.GetContextAsync();
        context.Response.StatusCode = status;
        context.Response.ContentLength64 = body.Length;
        try
        {
            if (stall)
            {
                await context.Response.OutputStream.WriteAsync(body.AsMemory(0, 4));
                await context.Response.OutputStream.FlushAsync();
                await Task.Delay(350);
            }
            else await context.Response.OutputStream.WriteAsync(body);
        }
        finally { if (stall) context.Response.Abort(); else context.Response.Close(); }
    });
    using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(10));
    await test(timeout.Token);
    await server.WaitAsync(TimeSpan.FromSeconds(10));
}
sealed class NoProgress : IProgress<(long Bytes, long? Total)> { public void Report((long Bytes, long? Total) value) { } }
