using System.Buffers.Binary;
using System.IO.Pipes;
using System.Threading.Channels;
using Google.Protobuf;
using Translator.Protocol;

namespace Translator.Desktop.Core;

/// <summary>
/// Named-pipe client for <c>translator-core</c>. Every frame is a 4-byte
/// big-endian length prefix followed by one protobuf <see cref="Envelope"/>.
/// </summary>
public sealed class CoreClient : IAsyncDisposable
{
    private const int MaxFrameBytes = 4 * 1024 * 1024;
    private static readonly TimeSpan ConnectTimeout = TimeSpan.FromSeconds(3);

    private readonly Channel<Envelope> _outbox =
        Channel.CreateUnbounded<Envelope>(new UnboundedChannelOptions { SingleReader = true });

    private readonly object _gate = new();
    private readonly Dictionary<uint, PendingRequest> _pending = new();

    private NamedPipeClientStream? _pipe;
    private CancellationTokenSource? _cts;
    private Task? _readLoop;
    private Task? _writeLoop;

    /// <summary>Raised for unsolicited messages (subtitles, status, metrics, errors).</summary>
    public event Action<Envelope>? EnvelopeReceived;

    /// <summary>Raised when the pipe dies unexpectedly.</summary>
    public event Action<Exception>? Faulted;

    public bool IsConnected => _pipe is { IsConnected: true };

    private sealed record PendingRequest(
        TaskCompletionSource<Envelope> Completion,
        Func<Envelope, bool> Matches);

    public async Task ConnectAsync(string pipeName, CancellationToken cancellationToken = default)
    {
        var pipe = new NamedPipeClientStream(
            ".",
            pipeName,
            PipeDirection.InOut,
            PipeOptions.Asynchronous | PipeOptions.CurrentUserOnly);

        await pipe.ConnectAsync((int)ConnectTimeout.TotalMilliseconds, cancellationToken)
            .ConfigureAwait(false);

        var cts = new CancellationTokenSource();
        _pipe = pipe;
        _cts = cts;
        _readLoop = Task.Run(() => ReadLoopAsync(pipe, cts.Token), CancellationToken.None);
        _writeLoop = Task.Run(() => WriteLoopAsync(pipe, cts.Token), CancellationToken.None);
    }

    /// <summary>Fire-and-forget send (used for audio frames).</summary>
    public void Post(Envelope envelope) => _outbox.Writer.TryWrite(envelope);

    /// <summary>Send a request and wait for the matching response.</summary>
    public async Task<Envelope?> RequestAsync(
        Envelope request,
        Func<Envelope, bool> matches,
        TimeSpan timeout,
        CancellationToken cancellationToken = default)
    {
        var completion = new TaskCompletionSource<Envelope>(TaskCreationOptions.RunContinuationsAsynchronously);
        lock (_gate)
        {
            _pending[request.Seq] = new PendingRequest(completion, matches);
        }

        Post(request);

        using var delayCts = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        var delay = Task.Delay(timeout, delayCts.Token);
        var finished = await Task.WhenAny(completion.Task, delay).ConfigureAwait(false);

        lock (_gate)
        {
            _pending.Remove(request.Seq);
        }

        if (finished != completion.Task)
        {
            completion.TrySetCanceled(delayCts.IsCancellationRequested
                ? delayCts.Token
                : new CancellationToken(true));
            return null;
        }

        return await completion.Task.ConfigureAwait(false);
    }

    private async Task WriteLoopAsync(Stream stream, CancellationToken cancellationToken)
    {
        try
        {
            await foreach (var envelope in _outbox.Reader.ReadAllAsync(cancellationToken).ConfigureAwait(false))
            {
                var frame = Encode(envelope);
                await stream.WriteAsync(frame, cancellationToken).ConfigureAwait(false);
                await stream.FlushAsync(cancellationToken).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException)
        {
        }
        catch (Exception ex)
        {
            Faulted?.Invoke(ex);
        }
    }

    private async Task ReadLoopAsync(Stream stream, CancellationToken cancellationToken)
    {
        var header = new byte[4];
        try
        {
            while (!cancellationToken.IsCancellationRequested)
            {
                await stream.ReadExactlyAsync(header, cancellationToken).ConfigureAwait(false);
                var length = BinaryPrimitives.ReadUInt32BigEndian(header);
                if (length > MaxFrameBytes)
                {
                    throw new InvalidDataException($"frame of {length} bytes exceeds the {MaxFrameBytes} byte limit");
                }

                var payload = new byte[length];
                await stream.ReadExactlyAsync(payload, cancellationToken).ConfigureAwait(false);

                var envelope = Envelope.Parser.ParseFrom(payload);
                if (TryCompletePending(envelope))
                {
                    continue;
                }

                EnvelopeReceived?.Invoke(envelope);
            }
        }
        catch (OperationCanceledException)
        {
        }
        catch (EndOfStreamException)
        {
        }
        catch (Exception ex)
        {
            Faulted?.Invoke(ex);
        }
    }

    private bool TryCompletePending(Envelope envelope)
    {
        List<PendingRequest>? matched = null;
        lock (_gate)
        {
            var keys = _pending.Keys.ToList();
            foreach (var key in keys)
            {
                if (_pending[key].Matches(envelope))
                {
                    (matched ??= new List<PendingRequest>()).Add(_pending[key]);
                    _pending.Remove(key);
                }
            }
        }

        if (matched is null)
        {
            return false;
        }

        foreach (var request in matched)
        {
            request.Completion.TrySetResult(envelope);
        }
        return true;
    }

    private static byte[] Encode(Envelope envelope)
    {
        var body = envelope.ToByteArray();
        var frame = new byte[4 + body.Length];
        BinaryPrimitives.WriteUInt32BigEndian(frame, (uint)body.Length);
        body.CopyTo(frame, 4);
        return frame;
    }

    public async ValueTask DisposeAsync()
    {
        _outbox.Writer.TryComplete();
        _cts?.Cancel();

        foreach (var task in new[] { _readLoop, _writeLoop })
        {
            if (task is null)
            {
                continue;
            }
            try
            {
                await task.ConfigureAwait(false);
            }
            catch
            {
                // Handled through the Faulted event.
            }
        }

        _cts?.Dispose();
        if (_pipe is not null)
        {
            await _pipe.DisposeAsync().ConfigureAwait(false);
            _pipe = null;
        }
    }
}
