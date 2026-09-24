using System.Runtime.InteropServices;

namespace Translator.Desktop.Native;

/// <summary>
/// Minimal Win32 surface for making the overlay click-through and keying out its
/// black background. WinUI 3 has no managed API for either.
/// </summary>
internal static class Win32
{
    public const int GwlExStyle = -20;

    public const long WsExLayered = 0x0008_0000L;
    public const long WsExTransparent = 0x0000_0020L;
    public const long WsExToolWindow = 0x0000_0080L;

    public const uint LwaColorKey = 0x0000_0001;

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
