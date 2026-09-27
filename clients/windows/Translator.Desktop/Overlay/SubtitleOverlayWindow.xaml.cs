using Microsoft.UI;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Input;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Documents;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Translator.Desktop.Native;
using Translator.Desktop.Services;
using Windows.Graphics;

namespace Translator.Desktop.Overlay;

/// <summary>
/// Caption overlay window. Locked mode (default) is always-on-top and
/// click-through; unlocked mode exposes drag-to-move on the card body and
/// edge/corner grips for resizing, with the frame persisted across runs.
/// Rendering is driven entirely by the core: the client just paints whatever
/// <c>SubtitleEvent.lines</c> contains.
/// </summary>
public sealed partial class SubtitleOverlayWindow : Window
{
    private static readonly SolidColorBrush CommittedBrush =
        new(Windows.UI.Color.FromArgb(0xFF, 0xF5, 0xF5, 0xF5));

    private static readonly SolidColorBrush PartialBrush =
        new(Windows.UI.Color.FromArgb(0xFF, 0xA8, 0xA8, 0xA8));

    private const int MinWidth = 280;
    private const int MinHeight = 120;
    private const int DefaultHeight = 260;

    private readonly nint _hwnd;
    private readonly AppWindow _appWindow;
    private readonly OverlaySettings _settings;
    private readonly DispatcherQueueTimer _saveTimer;

    private string? _resizeMode;
    private Win32.NativePoint _dragStartCursor;
    private Win32.NativeRect _dragStartRect;

    public SubtitleOverlayWindow()
    {
        InitializeComponent();

        _hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
        var windowId = Win32Interop.GetWindowIdFromWindow(_hwnd);
        _appWindow = AppWindow.GetFromWindowId(windowId);
        _settings = OverlaySettingsStore.Load();

        ConfigurePresenter();

        var workArea = DisplayArea.GetFromWindowId(windowId, DisplayAreaFallback.Primary).WorkArea;
        if (_settings.HasFrame && _settings.Width >= MinWidth && _settings.Height >= MinHeight)
        {
            _appWindow.MoveAndResize(new RectInt32(_settings.X, _settings.Y, _settings.Width, _settings.Height));
        }
        else
        {
            PositionBottomCenter(workArea);
        }

        // Locked by default so captions never steal clicks while watching.
        SetClickThrough(_settings.Locked);

        // WinUI's compositor does not reliably honor layered-window color keys.
        // Clip the HWND itself so neither the unused surface nor its frame can show.
        Root.Loaded += (_, _) => UpdateWindowRegion();
        Root.SizeChanged += (_, _) => UpdateWindowRegion();
        CaptionBorder.SizeChanged += (_, _) => UpdateWindowRegion();
        Activated += (_, _) => UpdateWindowRegion();
        UpdateWindowRegion();

        // Persist the frame the user dragged to (debounced: our own resize
        // loop raises a Changed storm).
        _saveTimer = DispatcherQueue.CreateTimer();
        _saveTimer.Interval = TimeSpan.FromMilliseconds(500);
        _saveTimer.IsRepeating = false;
        _saveTimer.Tick += (_, _) => OverlaySettingsStore.Save(CurrentSettings());
        _appWindow.Changed += (_, _) => _saveTimer.Start();
    }

    private void ConfigurePresenter()
    {
        var presenter = _appWindow.Presenter as OverlappedPresenter ?? OverlappedPresenter.Create();
        presenter.SetBorderAndTitleBar(false, false);
        presenter.IsResizable = false; // resizing goes through the grips
        presenter.IsMinimizable = false;
        presenter.IsMaximizable = false;
        presenter.IsAlwaysOnTop = _settings.Topmost;

        if (!ReferenceEquals(_appWindow.Presenter, presenter))
        {
            _appWindow.SetPresenter(presenter);
        }

        _appWindow.IsShownInSwitchers = false;
    }

    private void PositionBottomCenter(Windows.Graphics.RectInt32 workArea)
    {
        var width = Math.Min(1100, Math.Max(400, workArea.Width - 80));
        var x = workArea.X + (workArea.Width - width) / 2;
        var y = workArea.Y + workArea.Height - DefaultHeight - 40;
        _appWindow.MoveAndResize(new RectInt32(x, y, width, DefaultHeight));
    }

    private OverlaySettings CurrentSettings()
    {
        Win32.GetWindowRect(_hwnd, out var rect);
        return new OverlaySettings(
            Topmost: (_appWindow.Presenter as OverlappedPresenter)?.IsAlwaysOnTop ?? true,
            Locked: _settings.Locked,
            HasFrame: true,
            X: rect.Left, Y: rect.Top,
            Width: rect.Right - rect.Left, Height: rect.Bottom - rect.Top);
    }

    /// <summary>Switch the always-on-top stacking level.</summary>
    public void SetTopmost(bool topmost)
    {
        if (_appWindow.Presenter is OverlappedPresenter presenter)
        {
            presenter.IsAlwaysOnTop = topmost;
        }
        _settings = CurrentSettings();
        OverlaySettingsStore.Save(_settings);
    }

    /// <summary>
    /// Toggles lock state: locked = mouse pass-through captions; unlocked =
    /// interactive (drag to move, grips to resize). The native region keeps
    /// the visible window confined to the caption card regardless of
    /// color-key support.
    /// </summary>
    public void SetClickThrough(bool enabled)
    {
        _settings = _settings with { Locked = enabled };
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
        Grips.Visibility = enabled ? Visibility.Collapsed : Visibility.Visible;
        UpdateWindowRegion();
        OverlaySettingsStore.Save(CurrentSettings());
    }

    // --- move -------------------------------------------------------------

    private void OnCardPointerPressed(object sender, PointerRoutedEventArgs e)
    {
        if (_settings.Locked) return;
        // Hand the drag to the system: native move loop, no manual tracking.
        Win32.ReleaseCapture();
        Win32.SendMessage(_hwnd, Win32.WmNclButtonDown, Win32.HtCaption, 0);
        e.Handled = true;
    }

    // --- resize grips -------------------------------------------------------

    private void OnGripPressed(object sender, PointerRoutedEventArgs e)
    {
        if (sender is not FrameworkElement { Tag: string mode }) return;
        _resizeMode = mode;
        _ = Win32.GetCursorPos(out _dragStartCursor);
        _ = Win32.GetWindowRect(_hwnd, out _dragStartRect);
        ((FrameworkElement)sender).CapturePointer(e.Pointer);
        e.Handled = true;
    }

    private void OnGripMoved(object sender, PointerRoutedEventArgs e)
    {
        if (_resizeMode is null) return;
        _ = Win32.GetCursorPos(out var now);
        var dx = now.X - _dragStartCursor.X;
        var dy = now.Y - _dragStartCursor.Y;

        var left = _dragStartRect.Left;
        var top = _dragStartRect.Top;
        var right = _dragStartRect.Right;
        var bottom = _dragStartRect.Bottom;
        if (_resizeMode.Contains("Left")) left = Math.Min(left + dx, right - MinWidth);
        if (_resizeMode.Contains("Right")) right = Math.Max(right + dx, left + MinWidth);
        if (_resizeMode.Contains("Top")) top = Math.Min(top + dy, bottom - MinHeight);
        if (_resizeMode.Contains("Bottom")) bottom = Math.Max(bottom + dy, top + MinHeight);

        _appWindow.MoveAndResize(new RectInt32(left, top, right - left, bottom - top));
        e.Handled = true;
    }

    private void OnGripReleased(object sender, PointerRoutedEventArgs e)
    {
        if (_resizeMode is null) return;
        _resizeMode = null;
        if (sender is FrameworkElement element) element.ReleasePointerCapture(e.Pointer);
        OverlaySettingsStore.Save(CurrentSettings());
        e.Handled = true;
    }

    private void OnGripEntered(object sender, PointerRoutedEventArgs e)
    {
        if (_resizeMode is not null) return;
        var shape = (sender as FrameworkElement)?.Tag as string ?? "";
        ProtectedCursor = InputSystemCursor.Create(shape switch
        {
            "Left" or "Right" => InputSystemCursorShape.SizeWestEast,
            "Top" or "Bottom" => InputSystemCursorShape.SizeNorthSouth,
            "TopLeft" or "BottomRight" => InputSystemCursorShape.SizeNorthwestSoutheast,
            _ => InputSystemCursorShape.SizeNortheastSouthwest,
        });
    }

    private void OnGripExited(object sender, PointerRoutedEventArgs e)
    {
        ProtectedCursor = InputSystemCursor.Create(InputSystemCursorShape.Arrow);
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
