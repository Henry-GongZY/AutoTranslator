using System.Diagnostics;
using Google.Protobuf;
using Translator.Desktop.Audio;
using Translator.Desktop.Core;
using Translator.Protocol;

namespace Translator.Desktop.Services;

public sealed record SubtitleOptions(int MaxLines = 2, int MaxCharsPerLine = 42);

/// <summary>
/// Glue between the audio capture, the core process and the overlay window.
/// </summary>
public sealed class SubtitleController : IAsyncDisposable
{
    public const string DefaultPipeName = "translator-core-v1";

    /// <summary>Must match <c>translator_protocol::PROTOCOL_VERSION</c>.</summary>
    private const uint ProtocolVersion = 1;

    private static readonly TimeSpan RequestTimeout = TimeSpan.FromSeconds(5);
    private static readonly TimeSpan ConnectBudget = TimeSpan.FromSeconds(10);

    private readonly CoreProcess _core;
    private readonly Stopwatch _clock = new();
    private CoreClient? _client;
    private SystemAudioCapture? _capture;
    private int _seq;
    private bool _running;
    private string _sessionId = "desktop-1";

    /// <summary>Lines to render, and whether the last one is still provisional.</summary>
    public event Action<IReadOnlyList<string>, bool>? SubtitlesChanged;

    public event Action<string>? StatusChanged;
    public event Action<MetricsEvent>? MetricsReceived;

    public SubtitleController(string pipeName = DefaultPipeName, string logLevel = "info")
    {
        _core = new CoreProcess(pipeName);
        LogLevel = logLevel;
    }

    public string LogLevel { get; }

    public bool IsRunning => _running;

    public async Task StartAsync(SubtitleOptions options, CancellationToken cancellationToken = default)
    {
        if (_running)
        {
            return;
        }

        var corePath = CoreProcess.FindCoreExecutable()
            ?? throw new FileNotFoundException(
                "找不到 translator-core.exe。请先运行 `cargo build`，或设置 TRANSLATOR_CORE 环境变量指向它。");

        StatusChanged?.Invoke($"启动 core：{corePath}");
        _core.Start(corePath, LogLevel);

        _client = CreateClient();
        await ConnectWithRetryAsync(_client, cancellationToken).ConfigureAwait(false);

        var handshake = new Envelope
        {
            Seq = NextSeq(),
            HandshakeRequest = new HandshakeRequest
            {
                ProtocolVersion = ProtocolVersion,
                ClientId = "translator-desktop-windows",
                ClientVersion = "0.1.0",
            },
        };
        var handshakeReply = await _client
            .RequestAsync(handshake, IsHandshakeResponse, RequestTimeout, cancellationToken)
            .ConfigureAwait(false) ?? throw new TimeoutException("core 未响应握手请求");
        if (!handshakeReply.HandshakeResponse.Accepted)
        {
            throw new InvalidOperationException($"core 拒绝了握手：{handshakeReply.HandshakeResponse.Error}");
        }

        var capture = new SystemAudioCapture();
        capture.DataAvailable += OnAudioData;
        capture.Start();

        var request = new Envelope
        {
            Seq = NextSeq(),
            StartSessionRequest = new StartSessionRequest
            {
                SessionId = _sessionId,
                InputFormat = new AudioFormat
                {
                    SampleRate = (uint)capture.SampleRate,
                    Channels = (uint)capture.Channels,
                    Format = capture.SampleFormat,
                },
                TargetSampleRate = 16_000,
                Asr = new AsrConfig { Provider = "mock" },
                Translation = new TranslationConfig { Provider = "none" },
                Subtitle = new SubtitleConfig
                {
                    MaxLines = (uint)Math.Max(1, options.MaxLines),
                    MaxCharsPerLine = (uint)Math.Max(10, options.MaxCharsPerLine),
                    PartialIntervalMs = 120,
                },
            },
        };

        var started = await _client
            .RequestAsync(request, IsStartSessionResponse, RequestTimeout, cancellationToken)
            .ConfigureAwait(false) ?? throw new TimeoutException("core 未响应会话启动请求");

        if (!started.StartSessionResponse.Accepted)
        {
            capture.Dispose();
            throw new InvalidOperationException($"core 拒绝了会话：{started.StartSessionResponse.Error}");
        }

        _capture = capture;
        _clock.Restart();
        _running = true;
        StatusChanged?.Invoke($"正在监听系统音频：{capture.SampleRate} Hz / {capture.Channels} 声道");
    }

    public async Task SetPausedAsync(bool paused, CancellationToken cancellationToken = default)
    {
        if (!_running || _client is null)
        {
            return;
        }

        var request = new Envelope
        {
            Seq = NextSeq(),
            SetPausedRequest = new SetPausedRequest { SessionId = _sessionId, Paused = paused },
        };
        await _client
            .RequestAsync(request, IsSetPausedResponse, RequestTimeout, cancellationToken)
            .ConfigureAwait(false);
        StatusChanged?.Invoke(paused ? "已暂停" : "已继续");
    }

    public async Task StopAsync()
    {
        _running = false;

        if (_capture is not null)
        {
            _capture.DataAvailable -= OnAudioData;
            _capture.Dispose();
            _capture = null;
        }

        if (_client is not null)
        {
            try
            {
                var stop = new Envelope
                {
                    Seq = NextSeq(),
                    StopSessionRequest = new StopSessionRequest { SessionId = _sessionId },
                };
                await _client.RequestAsync(stop, IsStopSessionResponse, RequestTimeout).ConfigureAwait(false);
            }
            catch
            {
                // The core may already be gone; nothing left to negotiate.
            }

            await _client.DisposeAsync().ConfigureAwait(false);
            _client = null;
        }

        _core.Kill();
        StatusChanged?.Invoke("已停止");
    }

    public async ValueTask DisposeAsync() => await StopAsync().ConfigureAwait(false);

    private CoreClient CreateClient()
    {
        var client = new CoreClient();
        client.EnvelopeReceived += OnEnvelope;
        client.Faulted += OnFaulted;
        return client;
    }

    private async Task ConnectWithRetryAsync(CoreClient client, CancellationToken cancellationToken)
    {
        var deadline = DateTime.UtcNow + ConnectBudget;
        while (true)
        {
            cancellationToken.ThrowIfCancellationRequested();
            try
            {
                await client.ConnectAsync(_core.PipeName, cancellationToken).ConfigureAwait(false);
                return;
            }
            catch when (DateTime.UtcNow < deadline)
            {
                await Task.Delay(100, cancellationToken).ConfigureAwait(false);
            }
        }
    }

    private void OnAudioData(byte[] pcm, int frames)
    {
        if (!_running || _capture is null || _client is null)
        {
            return;
        }

        _client.Post(new Envelope
        {
            Seq = NextSeq(),
            AudioFrame = new AudioFrame
            {
                SessionId = _sessionId,
                TimestampUs = (ulong)(_clock.Elapsed.TotalMilliseconds * 1000.0),
                Format = new AudioFormat
                {
                    SampleRate = (uint)_capture.SampleRate,
                    Channels = (uint)_capture.Channels,
                    Format = _capture.SampleFormat,
                },
                Frames = (uint)frames,
                Pcm = ByteString.CopyFrom(pcm),
            },
        });
    }

    private void OnEnvelope(Envelope envelope)
    {
        switch (envelope.PayloadCase)
        {
            case Envelope.PayloadOneofCase.SubtitleEvent:
                var subtitle = envelope.SubtitleEvent;
                SubtitlesChanged?.Invoke(subtitle.Lines.ToArray(), subtitle.Kind == SubtitleKind.Partial);
                break;

            case Envelope.PayloadOneofCase.StatusEvent:
                StatusChanged?.Invoke($"{envelope.StatusEvent.Status}：{envelope.StatusEvent.Detail}");
                break;

            case Envelope.PayloadOneofCase.MetricsEvent:
                MetricsReceived?.Invoke(envelope.MetricsEvent);
                break;

            case Envelope.PayloadOneofCase.ErrorEvent:
                StatusChanged?.Invoke(
                    $"错误 [{(ErrorCode)envelope.ErrorEvent.Code}] {envelope.ErrorEvent.Message}");
                break;
        }
    }

    private void OnFaulted(Exception exception) => StatusChanged?.Invoke($"连接中断：{exception.Message}");

    private uint NextSeq() => (uint)Interlocked.Increment(ref _seq);

    private static bool IsHandshakeResponse(Envelope e) =>
        e.PayloadCase == Envelope.PayloadOneofCase.HandshakeResponse;

    private static bool IsStartSessionResponse(Envelope e) =>
        e.PayloadCase == Envelope.PayloadOneofCase.StartSessionResponse;

    private static bool IsStopSessionResponse(Envelope e) =>
        e.PayloadCase == Envelope.PayloadOneofCase.StopSessionResponse;

    private static bool IsSetPausedResponse(Envelope e) =>
        e.PayloadCase == Envelope.PayloadOneofCase.SetPausedResponse;
}
