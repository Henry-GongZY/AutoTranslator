using System.Diagnostics;
using System.IO;

namespace Translator.Desktop.Core;

/// <summary>
/// Launches and supervises the <c>translator-core</c> child process, the same way
/// Clash Desktop supervises its core. The core owns no UI and no audio APIs.
/// </summary>
public sealed class CoreProcess : IDisposable
{
    /// <summary>Bare pipe name; the full path is <c>\\.\pipe\{name}</c>.</summary>
    public string PipeName { get; }

    private Process? _process;

    public CoreProcess(string pipeName)
    {
        PipeName = pipeName;
    }

    public bool IsRunning => _process is { HasExited: false };

    /// <summary>Full pipe path handed to the core via <c>--pipe</c>.</summary>
    public string FullPipePath => $@"\\.\pipe\{PipeName}";

    /// <summary>
    /// Locates <c>translator-core.exe</c>: env override, next to the app, a
    /// <c>core\</c> subfolder, then any <c>target\{debug,release}</c> up the tree.
    /// </summary>
    public static string? FindCoreExecutable()
    {
        const string fileName = "translator-core.exe";

        var fromEnv = Environment.GetEnvironmentVariable("TRANSLATOR_CORE");
        if (!string.IsNullOrWhiteSpace(fromEnv) && File.Exists(fromEnv))
        {
            return fromEnv;
        }

        var baseDir = AppContext.BaseDirectory;
        foreach (var candidate in new[] { fileName, Path.Combine("core", fileName) })
        {
            var full = Path.Combine(baseDir, candidate);
            if (File.Exists(full))
            {
                return full;
            }
        }

        var dir = new DirectoryInfo(baseDir.TrimEnd(Path.DirectorySeparatorChar));
        while (dir is not null)
        {
            foreach (var profile in new[] { "debug", "release" })
            {
                var full = Path.Combine(dir.FullName, "target", profile, fileName);
                if (File.Exists(full))
                {
                    return full;
                }
            }
            dir = dir.Parent;
        }

        return null;
    }

    public Process Start(string executablePath, string logLevel = "info")
    {
        var startInfo = new ProcessStartInfo(executablePath)
        {
            UseShellExecute = false,
            CreateNoWindow = true,
            RedirectStandardError = true,
            RedirectStandardOutput = true,
            WorkingDirectory = Path.GetDirectoryName(executablePath) ?? AppContext.BaseDirectory,
        };
        startInfo.ArgumentList.Add("--pipe");
        startInfo.ArgumentList.Add(FullPipePath);
        startInfo.ArgumentList.Add("--log-level");
        startInfo.ArgumentList.Add(logLevel);

        _process = Process.Start(startInfo)
            ?? throw new InvalidOperationException($"failed to start translator-core at {executablePath}");
        return _process;
    }

    public void Kill()
    {
        if (_process is null)
        {
            return;
        }
        try
        {
            if (!_process.HasExited)
            {
                _process.Kill(entireProcessTree: true);
                _process.WaitForExit(2000);
            }
        }
        catch
        {
            // Process already gone; nothing to do.
        }
        finally
        {
            _process.Dispose();
            _process = null;
        }
    }

    public void Dispose() => Kill();
}
