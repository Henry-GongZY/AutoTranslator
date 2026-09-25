using Microsoft.UI.Xaml;
using Translator.Desktop.Overlay;
using Translator.Desktop.Services;
using Translator.Protocol;

namespace Translator.Desktop;

public sealed partial class MainWindow : Window
{
    private readonly SubtitleController _controller = new();
    private readonly SubtitleOverlayWindow _overlay;

    public MainWindow()
    {
        InitializeComponent();

        _overlay = new SubtitleOverlayWindow();
        _overlay.Activate();

        // Apply the initial checkbox state explicitly: the XAML `Checked` event
        // fires during InitializeComponent, before `_overlay` exists.
        _overlay.SetClickThrough(ClickThroughBox.IsChecked == true);

        _controller.SubtitlesChanged += OnSubtitlesChanged;
        _controller.StatusChanged += OnStatusChanged;
        _controller.MetricsReceived += OnMetricsReceived;

        Closed += OnClosed;
    }

    private async void OnStartClicked(object sender, RoutedEventArgs e)
    {
        StartButton.IsEnabled = false;
        try
        {
            var options = new SubtitleOptions(
                (int)Math.Round(MaxLinesBox.Value),
                (int)Math.Round(MaxCharsBox.Value));
            var provider = TagOf(ProviderBox) ?? "mock";
            var language = TagOf(LanguageBox) ?? "";

            // The Whisper path downloads the model and compiles kernels on the
            // first run; run it off the UI thread so the window stays responsive.
            // Do NOT use ConfigureAwait(false) here: the continuation touches UI
            // elements (StopButton, PauseSwitch) and must run on the UI thread.
            await Task.Run(() => _controller.StartAsync(options, provider, language));

            StopButton.IsEnabled = true;
            PauseSwitch.IsEnabled = true;
            PauseSwitch.IsOn = false;
        }
        catch (Exception ex)
        {
            DispatcherQueue.TryEnqueue(() => StatusText.Text = $"启动失败：{ex.Message}");
            StartButton.IsEnabled = true;
        }
    }

    private static string? TagOf(Microsoft.UI.Xaml.Controls.ComboBox box)
    {
        if (box.SelectedItem is Microsoft.UI.Xaml.Controls.ComboBoxItem item)
        {
            return item.Tag as string;
        }
        return null;
    }

    private async void OnStopClicked(object sender, RoutedEventArgs e)
    {
        StopButton.IsEnabled = false;
        PauseSwitch.IsEnabled = false;
        await _controller.StopAsync();
        StartButton.IsEnabled = true;
    }

    private async void OnPauseToggled(object sender, RoutedEventArgs e)
    {
        await _controller.SetPausedAsync(PauseSwitch.IsOn);
    }

    private void OnClickThroughChanged(object sender, RoutedEventArgs e)
    {
        // Also raised while XAML is loading, when `_overlay` is still null.
        _overlay?.SetClickThrough(ClickThroughBox.IsChecked == true);
    }

    private void OnSubtitlesChanged(IReadOnlyList<string> lines, bool hasPartial)
    {
        // Raised on the pipe reader thread; WinUI requires the UI thread.
        _overlay.DispatcherQueue.TryEnqueue(() => _overlay.SetLines(lines, hasPartial));
    }

    private void OnStatusChanged(string status)
    {
        DispatcherQueue.TryEnqueue(() => StatusText.Text = status);
    }

    private void OnMetricsReceived(MetricsEvent metrics)
    {
        var text = $"音频帧 {metrics.AudioFrames} · 丢弃 {metrics.DroppedFrames} · "
                   + $"累计 {metrics.AudioMs / 1000.0:F1}s · 语音占比 {metrics.SpeechRatio:P0} · "
                   + $"字幕事件 {metrics.SubtitleEvents}";
        DispatcherQueue.TryEnqueue(() => MetricsText.Text = text);
    }

    private async void OnClosed(object sender, WindowEventArgs args)
    {
        await _controller.StopAsync();
        await _controller.DisposeAsync();
        _overlay.Close();
    }
}
