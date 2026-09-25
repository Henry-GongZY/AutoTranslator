using System.Runtime.InteropServices;

namespace Translator.Desktop.Native;

/// <summary>
/// Minimal Win32 surface for making the overlay click-through and keying out its
/// black background. WinUI 3 has no managed API for either.
/// </summary>
internal static class Win32
{
    [DllImport("user32.dll")]
    public static extern uint GetDpiForWindow(nint hWnd);

    public const int GwlExStyle = -20;

    public const long WsExLayered = 0x0008_0000L;
    public const long WsExTransparent = 0x0000_0020L;
    public const long WsExToolWindow = 0x0000_0080L;

    public const uint LwaColorKey = 0x0000_0001;

    [DllImport("gdi32.dll", SetLastError = true)]
    public static extern nint CreateRoundRectRgn(int left, int top, int right, int bottom,
        int ellipseWidth, int ellipseHeight);

    [DllImport("gdi32.dll", SetLastError = true)]
    public static extern nint CreateRectRgn(int left, int top, int right, int bottom);

    [DllImport("gdi32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool DeleteObject(nint handle);

    [DllImport("user32.dll", SetLastError = true)]
    public static extern int SetWindowRgn(nint hwnd, nint region, bool redraw);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool GetWindowRect(nint hwnd, out NativeRect rect);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool ClientToScreen(nint hwnd, ref NativePoint point);

    [StructLayout(LayoutKind.Sequential)]
    public struct NativeRect { public int Left, Top, Right, Bottom; }

    [StructLayout(LayoutKind.Sequential)]
    public struct NativePoint { public int X, Y; }

    [DllImport("user32.dll", EntryPoint = "GetWindowLongPtr", SetLastError = true)]
    private static extern nint GetWindowLongPtr64(nint hWnd, int nIndex);

    [DllImport("user32.dll", EntryPoint = "GetWindowLong", SetLastError = true)]
    private static extern nint GetWindowLong32(nint hWnd, int nIndex);

    [DllImport("user32.dll", EntryPoint = "SetWindowLongPtr", SetLastError = true)]
    private static extern nint SetWindowLongPtr64(nint hWnd, int nIndex, nint dwNewLong);

    [DllImport("user32.dll", EntryPoint = "SetWindowLong", SetLastError = true)]
    private static extern nint SetWindowLong32(nint hWnd, int nIndex, nint dwNewLong);

    public static nint GetWindowLong(nint hWnd, int nIndex) =>
        nint.Size == 8 ? GetWindowLongPtr64(hWnd, nIndex) : GetWindowLong32(hWnd, nIndex);

    public static nint SetWindowLong(nint hWnd, int nIndex, nint dwNewLong) =>
        nint.Size == 8 ? SetWindowLongPtr64(hWnd, nIndex, dwNewLong) : SetWindowLong32(hWnd, nIndex, dwNewLong);

    [DllImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool SetLayeredWindowAttributes(nint hWnd, uint crKey, byte bAlpha, uint dwFlags);
}
