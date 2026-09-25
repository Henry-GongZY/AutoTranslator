using Microsoft.UI.Xaml;
using Translator.Desktop.Overlay;
using Translator.Desktop.Services;
using Translator.Protocol;
using Windows.Graphics;

namespace Translator.Desktop;

public sealed partial class MainWindow : Window
{
    private readonly SubtitleController _controller = new();
    private readonly SubtitleOverlayWindow _overlay;
    private bool _paused;
    private bool _closing;

    public MainWindow()
    {
        InitializeComponent();
        // AppWindow dimensions are physical pixels; preserve the intended size at high DPI.
        var hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
        var scale = Native.Win32.GetDpiForWindow(hwnd) / 96.0;
        var workArea = Microsoft.UI.Windowing.DisplayArea.GetFromWindowId(
            AppWindow.Id, Microsoft.UI.Windowing.DisplayAreaFallback.Primary).WorkArea;
        var width = Math.Min((int)(780 * scale), workArea.Width);
        var height = Math.Min((int)(900 * scale), workArea.Height);
        AppWindow.MoveAndResize(new RectInt32(
            workArea.X + (workArea.Width - width) / 2,
            workArea.Y + (workArea.Height - height) / 2, width, height));
        _overlay = new SubtitleOverlayWindow();
        _overlay.Activate();
        _overlay.SetClickThrough(ClickThroughBox.IsChecked == true);
        _controller.SubtitlesChanged += OnSubtitlesChanged;
        _controller.StatusChanged += OnStatusChanged;
        _controller.MetricsReceived += OnMetricsReceived;
        Closed += OnClosed;
        InitializeRecognitionSettings();
    }

    private void SetBusy(bool busy)
    {
        LoadingRing.IsActive = busy;
        LoadingRing.Visibility = busy ? Visibility.Visible : Visibility.Collapsed;
        StatusIcon.Visibility = busy ? Visibility.Collapsed : Visibility.Visible;
    }

    private void SetSettingsEnabled(bool enabled)
    {
        ProviderBox.IsEnabled = LanguageBox.IsEnabled = enabled;
        MaxLinesBox.IsEnabled = MaxCharsBox.IsEnabled = enabled;
        EngineBox.IsEnabled = ModelBox.IsEnabled = ModelDirectoryBox.IsEnabled = BrowseModelDirectory.IsEnabled = enabled;
        LanguageBox.IsEnabled = enabled && !(ModelBox.SelectedItem is WhisperModel { EnglishOnly: true } && TagOf(ProviderBox) == "whisper");
    }

    private async void OnStartClicked(object sender, RoutedEventArgs e)
    {
        _starting = true;
        StartButton.IsEnabled = false;
        SetSettingsEnabled(false);
        SetBusy(true);
        ErrorBar.IsOpen = false;
        StatusText.Text = "正在准备";
        StatusHint.Text = "正在连接识别引擎，请稍候。";
        PreviewText.Text = "等待识别音频…";
        CaptionState.Text = "等待语音";
        MetricsText.Text = "等待音频统计…";
        _overlay.SetLines(Array.Empty<string>(), false);
        try
        {
            var options = new SubtitleOptions(
                double.IsFinite(MaxLinesBox.Value) ? (int)Math.Round(MaxLinesBox.Value) : 2,
                double.IsFinite(MaxCharsBox.Value) ? (int)Math.Round(MaxCharsBox.Value) : 42);
            var provider = TagOf(ProviderBox) ?? "whisper";
            var language = TagOf(LanguageBox) ?? "";
            var model = ModelBox.SelectedItem as WhisperModel ?? ModelStore.Models[0];
            var engine = TagOf(EngineBox) ?? "cuda";
            var folder = ModelDirectoryBox.Text.Trim();
            if (provider == "whisper" && !ModelStore.IsReady(System.IO.Path.Combine(folder, model.FileName)))
                throw new InvalidOperationException("模型尚未下载完成，请先准备模型。");
            await Task.Run(() => _controller.StartAsync(options, provider, language, engine, model.FileName, folder));
            if (_closing) { await _controller.StopAsync(); return; }
            StopButton.IsEnabled = PauseButton.IsEnabled = true;
            _paused = false;
            PauseButton.Content = "暂停";
        }
        catch (Exception ex)
        {
            await _controller.StopAsync();
            if (_closing) return;
            ShowError($"启动失败：{ex.Message}");
            StartButton.IsEnabled = true;
            SetSettingsEnabled(true);
            UpdateStartAvailability();
        }
        finally
        {
            _starting = false;
            if (!_closing) { SetBusy(false); UpdateStartAvailability(); }
        }
    }

    private static string? TagOf(Microsoft.UI.Xaml.Controls.ComboBox box) =>
        (box.SelectedItem as Microsoft.UI.Xaml.Controls.ComboBoxItem)?.Tag as string;

    private async void OnStopClicked(object sender, RoutedEventArgs e)
    {
        StopButton.IsEnabled = PauseButton.IsEnabled = false;
        SetBusy(true);
        try
        {
            await _controller.StopAsync();
            _paused = false;
            PauseButton.Content = "暂停";
            CaptionState.Text = "已停止";
            _overlay.SetLines(Array.Empty<string>(), false);
        }
        catch (Exception ex) { ShowError($"停止失败：{ex.Message}"); }
        finally
        {
            SetBusy(false);
            StartButton.IsEnabled = true;
            SetSettingsEnabled(true);
            UpdateStartAvailability();
        }
    }

    private async void OnPauseClicked(object sender, RoutedEventArgs e)
    {
        PauseButton.IsEnabled = StopButton.IsEnabled = false;
        try
        {
            await _controller.SetPausedAsync(!_paused);
            _paused = !_paused;
            PauseButton.Content = _paused ? "继续" : "暂停";
            CaptionState.Text = _paused ? "已暂停" : "等待语音";
        }
        catch (Exception ex) { ShowError($"暂停操作失败：{ex.Message}"); }
        finally { PauseButton.IsEnabled = StopButton.IsEnabled = true; }
    }

    private void OnClickThroughChanged(object sender, RoutedEventArgs e) =>
        _overlay?.SetClickThrough(ClickThroughBox.IsChecked == true);

    private void OnSubtitlesChanged(IReadOnlyList<string> lines, bool hasPartial)
    {
        var simplified = lines.Select(ChineseConverter.ToSimplified).ToArray();
        DispatcherQueue.TryEnqueue(() =>
        {
            if (_closing) return;
            _overlay.SetLines(simplified, hasPartial);
            PreviewText.Text = simplified.Length == 0 ? "等待识别音频…" : string.Join("\n", simplified);
            PreviewText.Foreground = (Microsoft.UI.Xaml.Media.Brush)Application.Current.Resources[
                simplified.Length == 0 ? "TextFillColorSecondaryBrush" : "TextFillColorPrimaryBrush"];
            CaptionState.Text = hasPartial ? "正在识别" : simplified.Length == 0 ? "等待语音" : "已更新";
        });
    }

    private void ShowError(string detail)
    {
        DiagnosticText.Text = detail;
        ErrorBar.IsOpen = true;
        StatusText.Text = "需要检查";
        StatusHint.Text = "识别暂时遇到问题，请查看运行诊断。";
        CaptionState.Text = "识别异常";
    }

    private void OnStatusChanged(string status)
    {
        DispatcherQueue.TryEnqueue(() =>
        {
            if (_closing) return;
            if (status.StartsWith("错误") || status.StartsWith("连接中断"))
            {
                ShowError(status);
                return;
            }
            // Keep an error available for inspection until the next listening session.
            if (!ErrorBar.IsOpen) DiagnosticText.Text = status;
            if (status.StartsWith("正在监听") || status == "已继续")
            {
                StatusText.Text = "正在监听";
                StatusHint.Text = TagOf(ProviderBox) == "whisper"
                    ? $"{TagOf(EngineBox)?.ToUpperInvariant()} · {(ModelBox.SelectedItem as WhisperModel)?.Id} · 正在识别系统音频"
                    : "演示模式 · 正在生成模拟字幕";
            }
            else if (status == "已暂停")
            {
                StatusText.Text = "已暂停";
                StatusHint.Text = "点击继续，恢复字幕识别。";
            }
            else if (status == "已停止")
            {
                StatusText.Text = "已停止";
                StatusHint.Text = "随时开始下一次监听。";
            }
            else if (status.Contains("模型"))
            {
                StatusHint.Text = status;
            }
        });
    }

    private void OnMetricsReceived(MetricsEvent metrics)
    {
        var text = $"音频帧 {metrics.AudioFrames} · 丢弃 {metrics.DroppedFrames} · "
                   + $"累计 {metrics.AudioMs / 1000.0:F1}s · 语音占比 {metrics.SpeechRatio:P0} · "
                   + $"字幕事件 {metrics.SubtitleEvents}";
        DispatcherQueue.TryEnqueue(() => { if (!_closing) MetricsText.Text = text; });
    }

    private async void OnClosed(object sender, WindowEventArgs args)
    {
        _closing = true;
        _modelDownload?.Cancel();
        _controller.SubtitlesChanged -= OnSubtitlesChanged;
        _controller.StatusChanged -= OnStatusChanged;
        _controller.MetricsReceived -= OnMetricsReceived;
        _overlay.Close();
        await _controller.DisposeAsync();
    }
}
