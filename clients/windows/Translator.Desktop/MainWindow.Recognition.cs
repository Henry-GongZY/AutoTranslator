using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Translator.Desktop.Core;
using Translator.Desktop.Services;
using Windows.Storage.Pickers;

namespace Translator.Desktop;

public sealed partial class MainWindow
{
    private bool _recognitionInitialized;
    private bool _starting;
    private bool _modelReady;
    private bool _applyingLanguage;
    private CancellationTokenSource? _modelDownload;

    private void InitializeRecognitionSettings()
    {
        var settings = ModelStore.Load();
        ModelBox.ItemsSource = ModelStore.Models;
        ModelBox.SelectedItem = ModelStore.Models.FirstOrDefault(m => m.Id == settings.Model) ?? ModelStore.Models[0];
        SelectTag(ProviderBox, settings.Provider);
        SelectTag(EngineBox, settings.Engine);
        SelectTag(LanguageBox, settings.Language);
        ModelDirectoryBox.Text = string.IsNullOrWhiteSpace(settings.Directory) ? ModelStore.DefaultDirectory : settings.Directory;
        // Reuse the existing tiny model when upgrading from the original application.
        if (string.IsNullOrWhiteSpace(settings.Directory))
        {
            var legacy = CoreProcess.FindCoreExecutable();
            if (legacy is not null)
            {
                var legacyFolder = System.IO.Path.Combine(System.IO.Path.GetDirectoryName(legacy)!, "models");
                if (ModelStore.IsReady(System.IO.Path.Combine(legacyFolder, "ggml-tiny.bin"))) ModelDirectoryBox.Text = legacyFolder;
            }
        }
        _recognitionInitialized = true;
        _ = PrepareSelectedModelAsync();
    }

    private static void SelectTag(ComboBox box, string tag)
    {
        var item = box.Items.OfType<ComboBoxItem>().FirstOrDefault(i => (string?)i.Tag == tag);
        if (item is not null) box.SelectedItem = item;
    }

    private void SaveRecognitionSettings()
    {
        if (!_recognitionInitialized) return;
        ModelStore.Save(new(TagOf(ProviderBox) ?? "whisper", TagOf(EngineBox) ?? "cuda",
            (ModelBox.SelectedItem as WhisperModel)?.Id ?? "tiny", ModelDirectoryBox.Text.Trim(), TagOf(LanguageBox) ?? ""));
    }

    private async void OnRecognitionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_recognitionInitialized) await PrepareSelectedModelAsync();
    }

    private void OnLanguageChanged(object sender, SelectionChangedEventArgs e)
    {
        if (!_recognitionInitialized || _applyingLanguage) return;
        try { SaveRecognitionSettings(); }
        catch (Exception ex) { ModelStatus.Text = $"无法保存设置：{ex.Message}"; }
    }

    private void UpdateStartAvailability()
    {
        if (!_recognitionInitialized || _closing) return;
        var local = TagOf(ProviderBox) == "whisper";
        var installed = CoreProcess.FindCoreExecutable(local ? TagOf(EngineBox) : null) is not null;
        StartButton.IsEnabled = !_starting && !_controller.IsRunning && installed && (!local || _modelReady);
    }

    private async Task PrepareSelectedModelAsync()
    {
        _modelDownload?.Cancel();
        var cancellation = new CancellationTokenSource();
        _modelDownload = cancellation;
        _modelReady = false;
        ModelProgress.Visibility = CancelModelButton.Visibility = RetryModelButton.Visibility = Visibility.Collapsed;
        var local = TagOf(ProviderBox) == "whisper";
        WhisperSettings.Visibility = local ? Visibility.Visible : Visibility.Collapsed;
        try
        {
            var model = ModelBox.SelectedItem as WhisperModel ?? ModelStore.Models[0];
            _applyingLanguage = true;
            if (local && model.EnglishOnly) SelectTag(LanguageBox, "en");
            LanguageBox.IsEnabled = !local || !model.EnglishOnly;
            _applyingLanguage = false;
            SaveRecognitionSettings();
            var engine = TagOf(EngineBox) ?? "cuda";
            EngineHint.Text = CoreProcess.FindCoreExecutable(engine) is null
                ? $"未安装 {engine.ToUpperInvariant()} 引擎包，请安装后再开始监听。"
                : $"{engine.ToUpperInvariant()} 引擎包已安装。启动时会检查运行库与硬件兼容性。";
            UpdateStartAvailability();
            if (!local) return;
            var folder = ModelDirectoryBox.Text.Trim();
            if (!System.IO.Path.IsPathFullyQualified(folder)) throw new InvalidOperationException("请输入完整路径，或点击浏览选择模型文件夹。");
            var path = System.IO.Path.Combine(folder, model.FileName);
            if (!ModelStore.IsReady(path))
            {
                ModelStatus.Text = $"正在下载 {model.Id} · {model.Description}…";
                ModelProgress.IsIndeterminate = true;
                ModelProgress.Visibility = CancelModelButton.Visibility = Visibility.Visible;
                var lastUpdate = DateTime.MinValue;
                var progress = new Progress<(long Bytes, long? Total)>(value =>
                {
                    if (_closing || cancellation.IsCancellationRequested || _modelDownload != cancellation) return;
                    if ((DateTime.UtcNow - lastUpdate).TotalMilliseconds < 150) return;
                    lastUpdate = DateTime.UtcNow;
                    ModelProgress.IsIndeterminate = value.Total is null or <= 0;
                    if (value.Total > 0) ModelProgress.Value = value.Bytes * 100.0 / value.Total.Value;
                    ModelStatus.Text = $"正在下载 {model.Id} · {value.Bytes / 1048576.0:F1} MiB" +
                        (value.Total > 0 ? $" / {value.Total.Value / 1048576.0:F1} MiB（{ModelProgress.Value:F0}%）" : "");
                });
                await ModelStore.EnsureAsync(model, folder, progress, cancellation.Token);
            }
            cancellation.Token.ThrowIfCancellationRequested();
            _modelReady = true;
            ModelStatus.Text = $"模型已就绪 · {model.FileName}" + (model.EnglishOnly ? " · 仅支持英语" : "");
        }
        catch (OperationCanceledException)
        {
            if (_modelDownload == cancellation && !_closing) { ModelStatus.Text = "下载已取消或连接超时，可点击重试。"; RetryModelButton.Visibility = Visibility.Visible; }
        }
        catch (Exception ex)
        {
            if (_modelDownload == cancellation && !_closing) { ModelStatus.Text = $"模型准备失败：{ex.Message}"; RetryModelButton.Visibility = Visibility.Visible; }
        }
        finally
        {
            if (_modelDownload == cancellation && !_closing)
            {
                ModelProgress.Visibility = CancelModelButton.Visibility = Visibility.Collapsed;
                UpdateStartAvailability();
            }
            if (_modelDownload == cancellation) _modelDownload = null;
            cancellation.Dispose();
        }
    }

    private async void OnBrowseModelDirectory(object sender, RoutedEventArgs e)
    {
        try
        {
            var picker = new FolderPicker();
            picker.FileTypeFilter.Add("*");
            WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
            var folder = await picker.PickSingleFolderAsync();
            if (folder is null) return;
            ModelDirectoryBox.Text = folder.Path;
            await PrepareSelectedModelAsync();
        }
        catch (Exception ex) { ModelStatus.Text = $"无法选择文件夹：{ex.Message}"; }
    }
    private async void OnModelDirectoryCommitted(object sender, RoutedEventArgs e)
    {
        if (_recognitionInitialized && !_starting && !_controller.IsRunning) await PrepareSelectedModelAsync();
    }
    private async void OnOpenModelDirectory(object sender, RoutedEventArgs e)
    {
        try
        {
            var path = ModelDirectoryBox.Text.Trim();
            if (!System.IO.Path.IsPathFullyQualified(path)) throw new InvalidOperationException("请选择有效的完整路径。");
            System.IO.Directory.CreateDirectory(path);
            await Windows.System.Launcher.LaunchFolderAsync(await Windows.Storage.StorageFolder.GetFolderFromPathAsync(path));
        }
        catch (Exception ex) { ModelStatus.Text = $"无法打开文件夹：{ex.Message}"; }
    }
    private async void OnRetryModel(object sender, RoutedEventArgs e) => await PrepareSelectedModelAsync();
    private void OnCancelModel(object sender, RoutedEventArgs e) => _modelDownload?.Cancel();
}
