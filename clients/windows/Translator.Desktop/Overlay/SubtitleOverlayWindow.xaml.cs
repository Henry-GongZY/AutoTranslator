using Microsoft.UI;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Media;
using Translator.Desktop.Native;
using Windows.Graphics;

namespace Translator.Desktop.Overlay;

/// <summary>
/// Always-on-top, click-through caption window. Rendering is driven entirely by
/// the core: the client just paints whatever <c>SubtitleEvent.lines</c> contains.
/// </summary>
public sealed partial class SubtitleOverlayWindow : Window
{
    private static readonly SolidColorBrush CommittedBrush =
        new(Windows.UI.Color.FromArgb(0xFF, 0xF5, 0xF5, 0xF5));

    private static readonly SolidColorBrush PartialBrush =
        new(Windows.UI.Color.FromArgb(0xFF, 0xA8, 0xA8, 0xA8));

    private readonly nint _hwnd;
    private readonly AppWindow _appWindow;
    private readonly int _windowWidth;
    private readonly int _windowHeight = 260;

    public SubtitleOverlayWindow()
    {
        InitializeComponent();

        _hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
        var windowId = Win32Interop.GetWindowIdFromWindow(_hwnd);
        _appWindow = AppWindow.GetFromWindowId(windowId);

        ConfigurePresenter();

        var workArea = DisplayArea.GetFromWindowId(windowId, DisplayAreaFallback.Primary).WorkArea;
        _windowWidth = Math.Min(1100, Math.Max(400, workArea.Width - 80));
        PositionBottomCenter(workArea);

        SetClickThrough(true);
    }

    private void ConfigurePresenter()
    {
        var presenter = _appWindow.Presenter as OverlappedPresenter ?? OverlappedPresenter.Create();
        presenter.SetBorderAndTitleBar(false, false);
        presenter.IsResizable = false;
        presenter.IsMinimizable = false;
        presenter.IsMaximizable = false;
        presenter.IsAlwaysOnTop = true;

        if (!ReferenceEquals(_appWindow.Presenter, presenter))
        {
            _appWindow.SetPresenter(presenter);
        }

        _appWindow.IsShownInSwitchers = false;
    }

    private void PositionBottomCenter(Windows.Graphics.RectInt32 workArea)
    {
        var x = workArea.X + (workArea.Width - _windowWidth) / 2;
        var y = workArea.Y + workArea.Height - _windowHeight - 40;
        _appWindow.Move(new PointInt32(x, y));
        _appWindow.Resize(new SizeInt32(_windowWidth, _windowHeight));
    }

    /// <summary>
    /// Toggles mouse pass-through. The black colour key stays active either way,
    /// so the window remains visually transparent.
    /// </summary>
    public void SetClickThrough(bool enabled)
    {
        var style = Win32.GetWindowLong(_hwnd, Win32.GwlExStyle).ToInt64();
        style |= Win32.WsExLayered;
        if (enabled)
        {
            style |= Win32.WsExTransparent | Win32.WsExToolWindow;
        }
        else
        {
            style &= ~Win32.WsExTransparent;
            style &= ~Win32.WsExToolWindow;
        }

        Win32.SetWindowLong(_hwnd, Win32.GwlExStyle, new nint(style));
        Win32.SetLayeredWindowAttributes(_hwnd, 0x0000_0000u, 255, Win32.LwaColorKey);
    }

    /// <summary>
    /// Paint the caption block. Must be called on the window's UI thread.
    /// </summary>
    public void SetLines(IReadOnlyList<string> lines, bool hasPartial)
    {
        CaptionText.Inlines.Clear();

        if (lines.Count == 0)
        {
            CaptionBorder.Visibility = Visibility.Collapsed;
            return;
        }

        CaptionBorder.Visibility = Visibility.Visible;

        for (var i = 0; i < lines.Count; i++)
        {
            var last = i == lines.Count - 1;
            CaptionText.Inlines.Add(new Run
            {
                Text = last ? lines[i] : lines[i] + "\n",
                Foreground = hasPartial && last ? PartialBrush : CommittedBrush,
            });
        }
    }
}
