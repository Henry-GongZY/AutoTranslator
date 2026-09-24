using NAudio.Wave;
using ProtocolSampleFormat = Translator.Protocol.SampleFormat;

namespace Translator.Desktop.Audio;

/// <summary>
/// Captures the Windows audio output mix (what you hear) using WASAPI loopback.
/// This is the only platform-specific piece on the Windows side.
/// </summary>
public sealed class SystemAudioCapture : IDisposable
{
    private WasapiLoopbackCapture? _capture;
    private bool _disposed;

    /// <summary>Raw PCM bytes together with the per-channel frame count.</summary>
    public event Action<byte[], int>? DataAvailable;

    /// <summary>Raised when capture stops, carrying the error if there was one.</summary>
    public event Action<Exception?>? Stopped;

    public int SampleRate => _capture?.WaveFormat.SampleRate ?? 0;
    public int Channels => _capture?.WaveFormat.Channels ?? 0;
    public int BitsPerSample => _capture?.WaveFormat.BitsPerSample ?? 0;

    public ProtocolSampleFormat SampleFormat =>
        _capture is null ? ProtocolSampleFormat.Unspecified : Classify(_capture.WaveFormat);

    public void Start()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        if (_capture is not null)
        {
            return;
        }

        var capture = new WasapiLoopbackCapture();
        capture.DataAvailable += OnDataAvailable;
        capture.RecordingStopped += OnRecordingStopped;
        capture.StartRecording();
        _capture = capture;
    }

    public void Stop()
    {
        if (_capture is null)
        {
            return;
        }
        try
        {
            _capture.StopRecording();
        }
        catch
        {
            // Device may already be gone.
        }
    }

    private void OnDataAvailable(object? sender, WaveInEventArgs e)
    {
        if (_capture is null || e.BytesRecorded <= 0)
        {
            return;
        }

        var bytesPerFrame = Math.Max(1, _capture.WaveFormat.Channels * _capture.WaveFormat.BitsPerSample / 8);
        var buffer = new byte[e.BytesRecorded];
        Buffer.BlockCopy(e.Buffer, 0, buffer, 0, e.BytesRecorded);

        DataAvailable?.Invoke(buffer, e.BytesRecorded / bytesPerFrame);
    }

    private void OnRecordingStopped(object? sender, StoppedEventArgs e) => Stopped?.Invoke(e.Exception);

    private static ProtocolSampleFormat Classify(WaveFormat format) => format.Encoding switch
    {
        WaveFormatEncoding.IeeeFloat when format.BitsPerSample == 32 => ProtocolSampleFormat.F32,
        WaveFormatEncoding.Pcm when format.BitsPerSample == 16 => ProtocolSampleFormat.I16,
        _ => ProtocolSampleFormat.Unspecified,
    };

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }
        _disposed = true;

        if (_capture is not null)
        {
            _capture.DataAvailable -= OnDataAvailable;
            _capture.RecordingStopped -= OnRecordingStopped;
            try
            {
                _capture.StopRecording();
            }
            catch
            {
                // Ignore teardown races.
            }
            _capture.Dispose();
            _capture = null;
        }
    }
}
