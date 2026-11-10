using System;
using System.Diagnostics;
using System.IO;
using System.IO.Compression;
using System.Reflection;
using System.Windows.Forms;

internal static class Program
{
    private const string Version = "__VERSION__";

    [STAThread]
    private static void Main(string[] args)
    {
        string baseDir = Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "GuguClientForRoblox", "portable-" + Version);
        string exePath = Path.Combine(baseDir, "GuguClient.exe");
        bool extractOnly = Array.IndexOf(args, "--extract-only") >= 0;

        try
        {
            if (!File.Exists(exePath))
            {
                Directory.CreateDirectory(baseDir);
                string tmpZip = Path.Combine(Path.GetTempPath(), "gugu-app-" + Guid.NewGuid().ToString("N") + ".zip");
                try
                {
                    using (Stream input = Assembly.GetExecutingAssembly().GetManifestResourceStream("app.zip"))
                    {
                        if (input == null) throw new InvalidOperationException("Embedded payload missing.");
                        using (FileStream output = File.Create(tmpZip))
                            input.CopyTo(output);
                    }
                    ZipFile.ExtractToDirectory(tmpZip, baseDir);
                }
                finally
                {
                    try { if (File.Exists(tmpZip)) File.Delete(tmpZip); } catch { }
                }
            }
        }
        catch (Exception ex)
        {
            MessageBox.Show("Failed to extract Gugu Client for Roblox:\n" + ex.Message,
                "Gugu Client for Roblox (portable)", MessageBoxButtons.OK, MessageBoxIcon.Error);
            return;
        }

        if (extractOnly)
        {
            Console.WriteLine(exePath);
            return;
        }

        // Remove older extracted portable versions if not locked
        try
        {
            string parentDir = Path.GetDirectoryName(baseDir);
            if (!string.IsNullOrEmpty(parentDir) && Directory.Exists(parentDir))
            {
                foreach (string dir in Directory.GetDirectories(parentDir, "portable-*"))
                {
                    if (!string.Equals(dir, baseDir, StringComparison.OrdinalIgnoreCase))
                    {
                        try { Directory.Delete(dir, true); } catch { }
                    }
                }
            }
        }
        catch { }

        var start = new ProcessStartInfo
        {
            FileName = exePath,
            WorkingDirectory = baseDir,
            UseShellExecute = false,
        };
        start.EnvironmentVariables["GUGU_PORTABLE_EXE"] = Assembly.GetExecutingAssembly().Location;
        // forward arguments (skip nothing; launcher args are app args)
        start.Arguments = string.Join(" ", Array.ConvertAll(args, QuoteArg));
        try { Process.Start(start); }
        catch (Exception ex)
        {
            MessageBox.Show("Failed to start Gugu Client for Roblox:\n" + ex.Message,
                "Gugu Client for Roblox (portable)", MessageBoxButtons.OK, MessageBoxIcon.Error);
        }
    }

    private static string QuoteArg(string arg)
    {
        if (string.IsNullOrEmpty(arg)) return "\"\"";
        if (arg.IndexOfAny(new[] { ' ', '\t', '"' }) < 0) return arg;
        return "\"" + arg.Replace("\\", "\\\\").Replace("\"", "\\\"") + "\"";
    }
}
