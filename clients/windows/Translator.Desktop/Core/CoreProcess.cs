using System.Diagnostics;
using System.IO;
using System.Text;

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
    private Task? _stdoutReader;
    private Task? _stderrReader;

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
    public static string? FindCoreExecutable(string? engine = null)
    {
        const string fileName = "translator-core.exe";

        var fromEnv = Environment.GetEnvironmentVariable("TRANSLATOR_CORE");
        if (!string.IsNullOrWhiteSpace(fromEnv) && File.Exists(fromEnv))
        {
            return fromEnv;
        }

        var baseDir = AppContext.BaseDirectory;
        if (!string.IsNullOrEmpty(engine))
        {
            if (engine is not ("cpu" or "blas" or "vulkan" or "cuda")) return null;
            var root = new DirectoryInfo(baseDir);
            while (root is not null)
            {
                var packaged = Path.Combine(root.FullName, "engines", engine, fileName);
                if (File.Exists(packaged)) return packaged;
                root = root.Parent;
            }
            return null;
        }

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
            foreach (var profile in new[] { "release", "debug" })
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
            StandardOutputEncoding = Encoding.UTF8,
            StandardErrorEncoding = Encoding.UTF8,
            WorkingDirectory = Path.GetDirectoryName(executablePath) ?? AppContext.BaseDirectory,
        };
        startInfo.ArgumentList.Add("--pipe");
        startInfo.ArgumentList.Add(FullPipePath);
        startInfo.ArgumentList.Add("--log-level");
        startInfo.ArgumentList.Add(logLevel);

        // When the core is built with the CUDA backend it needs the CUDA runtime
        // DLLs (cudart/cublas) on PATH at startup. Prepend the toolkit bin dir
        // if present; this is a no-op for the CPU build.
        var cudaBin = Environment.GetEnvironmentVariable("CUDA_PATH");
        if (!string.IsNullOrWhiteSpace(cudaBin))
        {
            cudaBin = Path.Combine(cudaBin, "bin");
            if (Directory.Exists(cudaBin))
            {
                startInfo.Environment["PATH"] =
                    cudaBin + Path.PathSeparator + Environment.GetEnvironmentVariable("PATH");
            }
        }

        _process = Process.Start(startInfo)
            ?? throw new InvalidOperationException($"failed to start translator-core at {executablePath}");

        // Capture stdout/stderr to rotating log files in %LOCALAPPDATA%\Translator\logs.
        // The core logs everything to stderr, so this is essential for debugging.
        var logDir = Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "Translator",
            "logs");
        Directory.CreateDirectory(logDir);
        var timestamp = DateTime.Now.ToString("yyyyMMdd-HHmmss");
        var outLog = Path.Combine(logDir, $"core-stdout-{timestamp}.log");
        var errLog = Path.Combine(logDir, $"core-stderr-{timestamp}.log");

        _stdoutReader = Task.Run(() => CopyStream(_process.StandardOutput, outLog));
        _stderrReader = Task.Run(() => CopyStream(_process.StandardError, errLog));

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
            // Give the stream readers a moment to drain before disposal.
            try { _stdoutReader?.Wait(1000); } catch { /* ignored */ }
            try { _stderrReader?.Wait(1000); } catch { /* ignored */ }
            _process.Dispose();
            _process = null;
        }
    }

    public void Dispose() => Kill();

    private static void CopyStream(StreamReader reader, string logPath)
    {
        try
        {
            using var writer = new StreamWriter(logPath, false, Encoding.UTF8) { AutoFlush = true };
            while (!reader.EndOfStream)
            {
                var line = reader.ReadLine();
                if (line is not null)
                {
                    writer.WriteLine(line);
                }
            }
        }
        catch
        {
            // The process may be killed while we are still reading; ignore.
        }
    }
}
