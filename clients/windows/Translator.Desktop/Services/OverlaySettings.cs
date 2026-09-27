using System.IO;
using System.Text.Json;

namespace Translator.Desktop.Services;

/// <summary>
/// Persisted overlay window preferences: stacking, lock state and the window
/// frame the user dragged the caption box to.
/// </summary>
public sealed record OverlaySettings(
    bool Topmost = true,
    bool Locked = true,
    bool HasFrame = false,
    int X = 0, int Y = 0, int Width = 0, int Height = 0);

public static class OverlaySettingsStore
{
    private static readonly string AppDirectory = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Translator");
    private static readonly string SettingsFile = Path.Combine(AppDirectory, "overlay.json");

    public static OverlaySettings Load()
    {
        try { return JsonSerializer.Deserialize<OverlaySettings>(File.ReadAllText(SettingsFile)) ?? new(); }
        catch { return new(); }
    }

    public static void Save(OverlaySettings settings)
    {
        try
        {
            Directory.CreateDirectory(AppDirectory);
            File.WriteAllText(SettingsFile + ".tmp", JsonSerializer.Serialize(settings));
            File.Move(SettingsFile + ".tmp", SettingsFile, true);
        }
        catch
        {
            // Settings are best-effort; a read-only profile must not break captions.
        }
    }
}
