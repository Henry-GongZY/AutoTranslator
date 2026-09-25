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
        // WinUI's compositor does not reliably honor layered-window color keys.
        // Clip the HWND itself so neither the unused surface nor its frame can show.
        Root.Loaded += (_, _) => UpdateWindowRegion();
        Root.SizeChanged += (_, _) => UpdateWindowRegion();
        CaptionBorder.SizeChanged += (_, _) => UpdateWindowRegion();
        Activated += (_, _) => UpdateWindowRegion();
        UpdateWindowRegion();
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
    /// Toggles mouse pass-through. The native region keeps the visible window
    /// confined to the caption card regardless of color-key support.
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
        UpdateWindowRegion();
    }

    private void UpdateWindowRegion()
    {
        nint region;
        if (CaptionBorder.Visibility != Visibility.Visible ||
            CaptionBorder.ActualWidth <= 0 || CaptionBorder.ActualHeight <= 0)
        {
            region = Win32.CreateRectRgn(0, 0, 0, 0);
        }
        else
        {
            var origin = CaptionBorder.TransformToVisual(Root).TransformPoint(new Windows.Foundation.Point());
            var scale = Root.XamlRoot?.RasterizationScale ?? Win32.GetDpiForWindow(_hwnd) / 96.0;
            // Window regions use window coordinates, while XAML uses client coordinates.
            var client = new Win32.NativePoint();
            if (!Win32.ClientToScreen(_hwnd, ref client) || !Win32.GetWindowRect(_hwnd, out var window))
                return;
            var left = (int)Math.Round(origin.X * scale) + client.X - window.Left;
            var top = (int)Math.Round(origin.Y * scale) + client.Y - window.Top;
            var right = left + (int)Math.Ceiling(CaptionBorder.ActualWidth * scale);
            var bottom = top + (int)Math.Ceiling(CaptionBorder.ActualHeight * scale);
            var diameter = (int)Math.Round(CaptionBorder.CornerRadius.TopLeft * 2 * scale);
            region = Win32.CreateRoundRectRgn(left, top, right, bottom, diameter, diameter);
        }

        if (region == 0) return;
        // On success Windows owns the HRGN; only free it when transfer fails.
        if (Win32.SetWindowRgn(_hwnd, region, true) == 0)
            Win32.DeleteObject(region);
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
            UpdateWindowRegion();
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
        // Reflow before clipping: a shorter caption must not retain the old region.
        Root.UpdateLayout();
        UpdateWindowRegion();
    }
}
