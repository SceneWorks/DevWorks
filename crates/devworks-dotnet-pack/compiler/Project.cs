using System.Text.RegularExpressions;
using System.Xml;
using System.Xml.Linq;

namespace DevWorks.Dotnet;

internal sealed class Project(string path, bool visualBasic)
{
    internal readonly string Path = path;
    internal readonly bool VisualBasic = visualBasic;
    internal readonly Dictionary<string, string> Properties = new(StringComparer.OrdinalIgnoreCase);
    internal readonly List<(string Glob, bool Include)> CompileItems = [];
    internal readonly List<string> ProjectReferences = [];
    internal readonly List<string> Imports = [];
    internal readonly List<string> FriendAssemblies = [];
    internal readonly List<CapturedFile> Sources = [];
    internal bool HasPackages, Web, AspNet;
    internal bool Loose => Path.StartsWith("loose:", StringComparison.Ordinal);
    internal string Directory => Parent(Path);
    internal string Target => Get("TargetFramework", Loose ? "net10.0" : "");
    internal string Assembly => Get("AssemblyName", System.IO.Path.GetFileNameWithoutExtension(Path));
    internal string Get(string name, string fallback = "") => Properties.GetValueOrDefault(name, fallback);
    internal bool Flag(string name, bool fallback = false) => Get(name).ToLowerInvariant() switch
    {
        "true" or "enable" or "on" => true,
        "false" or "disable" or "off" => false,
        "" => fallback,
        _ => throw new UnsupportedProject(),
    };
    internal static string Parent(string path) => path.Contains('/') ? path[..path.LastIndexOf('/')] : "";
    internal static bool Within(string path, string directory) => directory.Length == 0
        || path.StartsWith(directory + "/", StringComparison.Ordinal);

    internal void Load(string project, IReadOnlyDictionary<string, CapturedFile> files)
    {
        var props = Nearby("Directory.Build.props", files);
        var targets = Nearby("Directory.Build.targets", files);
        if (props != null) Read(props, false);
        Read(project, true);
        if (targets != null) Read(targets, false);
        if (!Regex.IsMatch(Target, @"\Anet[0-9]+\.[0-9]+\z")
            || string.IsNullOrWhiteSpace(Assembly) || Assembly.Length > 240
            || Assembly.IndexOfAny(['/', '\\', ':', '\0']) >= 0)
            throw new UnsupportedProject();
    }

    private string? Nearby(string name, IReadOnlyDictionary<string, CapturedFile> files)
    {
        for (var folder = Directory; ; folder = Parent(folder))
        {
            var candidate = folder.Length == 0 ? name : folder + "/" + name;
            if (files.TryGetValue(candidate, out var file)) return file.Text;
            if (folder.Length == 0) return null;
        }
    }

    private void Read(string text, bool own)
    {
        try
        {
            using var reader = XmlReader.Create(new StringReader(text.TrimStart('\uFEFF')),
                new XmlReaderSettings { DtdProcessing = DtdProcessing.Prohibit, XmlResolver = null });
            var xml = XDocument.Load(reader).Root ?? throw new UnsupportedProject();
            var sdk = (string?)xml.Attribute("Sdk");
            if (xml.Name != "Project"
                || (own ? sdk is not ("Microsoft.NET.Sdk" or "Microsoft.NET.Sdk.Web") : sdk != null)
                || xml.Attributes().Any(a => a.Name != "Sdk"))
                throw new UnsupportedProject();
            Web |= sdk == "Microsoft.NET.Sdk.Web";
            AspNet |= Web;
            foreach (var group in xml.Elements())
            {
                if (group.Name != "PropertyGroup" && group.Name != "ItemGroup")
                    throw new UnsupportedProject();
                foreach (var element in group.Elements())
                {
                    var name = element.Name.LocalName;
                    if (name is "PackageReference" or "PackageVersion")
                    {
                        HasPackages |= name == "PackageReference";
                        continue;
                    }
                    if (group.Name == "PropertyGroup" && Informational.Contains(name)
                        || group.Name == "ItemGroup" && name is "None" or "Content" or "EmbeddedResource" or "Folder")
                        continue;
                    if (group.HasAttributes || element.Name.NamespaceName != ""
                        || element.Descendants().Any()
                        || element.Attributes().Any(a => a.Name.LocalName is not ("Include" or "Remove"))
                        || element.Value.Contains("$(")
                        || element.Attributes().Any(a => a.Value.Contains("$(") || a.Value.Contains("@(")))
                        throw new UnsupportedProject();
                    if (group.Name == "PropertyGroup" && Modeled.Contains(name) && !element.HasAttributes)
                    {
                        Properties[name] = element.Value.Trim();
                        continue;
                    }
                    var include = (string?)element.Attribute("Include");
                    var remove = (string?)element.Attribute("Remove");
                    switch (name)
                    {
                        case "Compile" when (include == null) != (remove == null):
                            foreach (var glob in (include ?? remove!).Split(';', StringSplitOptions.TrimEntries))
                                CompileItems.Add((Normalize(Directory, glob, true), include != null));
                            break;
                        case "ProjectReference" when include != null && remove == null:
                            ProjectReferences.Add(Normalize(Directory, include));
                            break;
                        case "FrameworkReference" when include == "Microsoft.AspNetCore.App" && remove == null:
                            AspNet = true;
                            break;
                        case "InternalsVisibleTo" when include != null && remove == null
                            && Regex.IsMatch(include, @"\A[\w. -]+\z"):
                            FriendAssemblies.Add(include);
                            break;
                        case "Using" when !VisualBasic && include != null && remove == null:
                        case "Import" when VisualBasic && include != null && remove == null:
                            Imports.Add(include);
                            break;
                        default:
                            throw new UnsupportedProject();
                    }
                }
            }
        }
        catch (XmlException)
        {
            throw new UnsupportedProject();
        }
    }

    private static readonly HashSet<string> Modeled = new(StringComparer.OrdinalIgnoreCase)
    {
        "AssemblyName", "RootNamespace", "TargetFramework", "LangVersion", "DefineConstants",
        "Nullable", "OutputType", "EnableDefaultItems", "EnableDefaultCompileItems",
        "ImplicitUsings", "DisableImplicitNamespaceImports", "OptionStrict", "OptionExplicit",
        "OptionInfer", "OptionCompare", "AllowUnsafeBlocks",
    };

    private static readonly HashSet<string> Informational = new(StringComparer.OrdinalIgnoreCase)
    {
        "Description", "Version", "AssemblyVersion", "FileVersion", "Authors", "Company",
        "PackageId", "IsPackable", "IsTestProject", "TreatWarningsAsErrors", "NoWarn",
        "GenerateDocumentationFile", "WarningLevel", "Deterministic", "GenerateAssemblyInfo",
        "DebugType", "Optimize", "ManagePackageVersionsCentrally", "UserSecretsId",
        "InvariantGlobalization",
    };

    private static string Normalize(string directory, string link, bool glob = false)
    {
        link = link.Replace('\\', '/');
        if (link.Length == 0 || link.Length > 240 || link.StartsWith('/')
            || link.Contains(':') || link.Contains('%') || (!glob && link.IndexOfAny(['*', '?', ';']) >= 0))
            throw new UnsupportedProject();
        List<string> parts = directory.Split('/', StringSplitOptions.RemoveEmptyEntries).ToList();
        foreach (var part in link.Split('/'))
        {
            if (part is "" or ".") continue;
            if (part == "..")
            {
                if (parts.Count == 0 || parts[^1].IndexOfAny(['*', '?']) >= 0) throw new UnsupportedProject();
                parts.RemoveAt(parts.Count - 1);
            }
            else parts.Add(part);
        }
        return parts.Count == 0 ? throw new UnsupportedProject() : string.Join('/', parts);
    }

    internal bool Includes(string file, bool owned)
    {
        var included = owned && Flag("EnableDefaultItems", true) && Flag("EnableDefaultCompileItems", true);
        foreach (var (glob, include) in CompileItems)
        {
            if (MatchGlob(file, glob)) included = include;
        }
        return included;
    }

    private static bool MatchGlob(string path, string glob)
    {
        if (glob.IndexOfAny(['*', '?']) < 0) return path == glob;
        var pattern = Regex.Escape(glob).Replace(@"\*\*/", "(?:.*/)?")
            .Replace(@"\*\*", ".*").Replace(@"\*", "[^/]*").Replace(@"\?", "[^/]");
        return Regex.IsMatch(path, "\\A" + pattern + "\\z", RegexOptions.CultureInvariant | RegexOptions.NonBacktracking);
    }
}
