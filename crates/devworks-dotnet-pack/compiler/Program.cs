using System.Runtime.InteropServices;
using System.Text.Json;
using Microsoft.CodeAnalysis;

namespace DevWorks.Dotnet;

internal record CapturedFile(string Unit, string Language, string Sha256, string Text);
internal record Runtime(string SdkVersion, string RuntimeVersion, string RuntimeDirectory,
    Dictionary<string, string> References);
internal record Capture(string RepositoryId, string Commit, string? CaptureDigest,
    CapturedFile[] Files, Runtime Runtime);

internal sealed class UnsupportedProject : Exception;
internal sealed class OutputBudget : Exception;

internal static class Program
{
    internal static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web);

    private static void Main(string[] args)
    {
        if (args.Length != 3) throw new ArgumentException("Expected source, output and fault paths.");
        try
        {
            using var source = File.OpenRead(args[0]);
            var capture = JsonSerializer.Deserialize<Capture>(source, Json)
                ?? throw new InvalidDataException();
            if (Environment.Version.ToString(3) != capture.Runtime.RuntimeVersion
                || Path.TrimEndingDirectorySeparator(RuntimeEnvironment.GetRuntimeDirectory())
                    != capture.Runtime.RuntimeDirectory
                || typeof(Compilation).Assembly.GetName().Version != new Version(5, 3, 0, 0))
            {
                File.WriteAllText(args[2], "dotnet-runtime-changed");
                return;
            }
            var units = new Workspace(capture).Index();
            using var result = new BoundedOutput(args[1]);
            JsonSerializer.Serialize(result, new { units }, Json);
        }
        catch (UnsupportedProject)
        {
            File.WriteAllText(args[2], "dotnet-project-unsupported");
        }
        catch (OutputBudget)
        {
            File.WriteAllText(args[2], "dotnet-output-budget");
        }
    }

    private sealed class BoundedOutput(string path) : FileStream(path, FileMode.CreateNew)
    {
        public override void Write(ReadOnlySpan<byte> buffer)
        {
            if (Position + buffer.Length > 8 * 1024 * 1024) throw new OutputBudget();
            base.Write(buffer);
        }
        public override void Write(byte[] array, int offset, int count)
        {
            if (Position + count > 8 * 1024 * 1024) throw new OutputBudget();
            base.Write(array, offset, count);
        }
    }
}
