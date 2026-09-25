using System.Runtime.InteropServices;
using System.Text;

namespace Translator.Desktop;

/// <summary>
/// Converts between Traditional and Simplified Chinese using the mapping tables
/// built into Windows (NLS), so no extra dependency or language pack is needed.
/// Whisper's multilingual model emits Traditional Chinese for the `zh` language,
/// so we normalize every subtitle line to Simplified Chinese on display.
/// </summary>
internal static class ChineseConverter
{
    // Map a locale-independent string to Simplified / Traditional Chinese.
    private const uint LCMAP_SIMPLIFIED_CHINESE = 0x02000000;
    private const uint LCMAP_TRADITIONAL_CHINESE = 0x04000000;

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern int LCMapStringEx(
        string? lpLocaleName,
        uint dwMapFlags,
        string lpSrcStr,
        int cchSrc,
        StringBuilder? lpDestStr,
        int cchDest,
        nint lpVersionInformation,
        nint lpReserved,
        nint sortHandle);

    /// <summary>Returns <paramref name="input"/> with any Traditional Chinese
    /// characters rewritten to their Simplified Chinese form. Non-Chinese text is
    /// returned unchanged.</summary>
    public static string ToSimplified(string input)
    {
        if (string.IsNullOrEmpty(input))
        {
            return input;
        }

        var dest = new StringBuilder(input.Length + 1);
        _ = LCMapStringEx(
            null,
            LCMAP_SIMPLIFIED_CHINESE,
            input,
            input.Length,
            dest,
            dest.Capacity,
            nint.Zero,
            nint.Zero,
            nint.Zero);
        return dest.ToString();
    }
}
