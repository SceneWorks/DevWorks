using System.Collections.Immutable;
using System.Text.RegularExpressions;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.Emit;
using Microsoft.CodeAnalysis.Text;
using CS = Microsoft.CodeAnalysis.CSharp;
using VB = Microsoft.CodeAnalysis.VisualBasic;

namespace DevWorks.Dotnet;

internal sealed class Workspace
{
    private readonly Capture capture;
    private readonly Dictionary<string, Project> projects = new(StringComparer.Ordinal);
    private readonly Dictionary<Project, Build> builds = [];
    private readonly HashSet<Project> visiting = [];
    private readonly Dictionary<string, ImmutableArray<MetadataReference>> frameworkReferences = [];
    private readonly Dictionary<Project, MetadataReference?> projectReferences = [];
    private readonly List<CapturedFile> excluded = [];
    private int emittedBytes;

    private sealed record Build(Compilation Compilation, Project[] Dependencies, bool MissingProjectReference);

    internal Workspace(Capture capture)
    {
        this.capture = capture;
        var files = new Dictionary<string, CapturedFile>(StringComparer.OrdinalIgnoreCase);
        foreach (var file in capture.Files)
            if (!files.TryAdd(file.Unit, file)) throw new UnsupportedProject();
        foreach (var file in capture.Files.Where(IsProject))
        {
            var project = new Project(file.Unit, file.Unit.EndsWith(".vbproj", StringComparison.OrdinalIgnoreCase));
            project.Load(file.Text, files);
            projects.Add(file.Unit, project);
            if (projects.Count > 128) throw new UnsupportedProject();
        }
        var declared = projects.Values.ToArray();
        foreach (var file in capture.Files.Where(x => x.Language != "dotnet-context"))
        {
            var candidates = declared.Where(x => x.VisualBasic == (file.Language == "visual-basic")).ToArray();
            var nearest = candidates.Where(x => Project.Within(file.Unit, x.Directory))
                .Select(x => x.Directory.Length).DefaultIfEmpty(-1).Max();
            var owners = candidates.Where(x => x.Includes(file.Unit,
                x.Directory.Length == nearest && Project.Within(file.Unit, x.Directory))).ToArray();
            if (owners.Length > 1) throw new UnsupportedProject();
            if (owners.Length == 1)
            {
                owners[0].Sources.Add(file);
            }
            else if (nearest >= 0)
            {
                excluded.Add(file);
            }
            else
            {
                var key = "loose:" + file.Language;
                if (!projects.TryGetValue(key, out var loose))
                {
                    loose = new Project(key, file.Language == "visual-basic");
                    loose.Properties["AssemblyName"] = "DevWorks.Loose." + file.Language;
                    projects.Add(key, loose);
                }
                loose.Sources.Add(file);
            }
        }
        if (projects.Values.Select(x => x.Assembly).Distinct(StringComparer.OrdinalIgnoreCase).Count()
            != projects.Count)
            throw new UnsupportedProject();
    }

    private static bool IsProject(CapturedFile file) => file.Language == "dotnet-context"
        && (file.Unit.EndsWith(".csproj", StringComparison.OrdinalIgnoreCase)
            || file.Unit.EndsWith(".vbproj", StringComparison.OrdinalIgnoreCase));

    internal object[] Index()
    {
        foreach (var project in projects.Values) Bind(project);
        var graph = new Graph(capture, builds.ToDictionary(
            build => build.Value.Compilation.Assembly.Identity, build => build.Key.Path));
        foreach (var (project, build) in builds)
        {
            var gap = build.MissingProjectReference ? "dotnet-project-reference-unavailable"
                : project.HasPackages ? "dotnet-external-assemblies-unavailable" : null;
            var trees = build.Compilation.SyntaxTrees.ToDictionary(x => x.FilePath);
            foreach (var file in project.Sources)
                graph.Add(file, project, build.Compilation.GetSemanticModel(trees[file.Unit]), gap);
        }
        foreach (var file in excluded) graph.Exclude(file);
        return graph.Result();
    }

    private Build Bind(Project project)
    {
        if (builds.TryGetValue(project, out var known)) return known;
        if (!visiting.Add(project)) throw new UnsupportedProject();
        var dependencies = new HashSet<Project>();
        var missing = false;
        foreach (var link in project.ProjectReferences)
        {
            if (!projects.TryGetValue(link, out var referenced)) throw new UnsupportedProject();
            var result = Bind(referenced);
            dependencies.Add(referenced);
            dependencies.UnionWith(result.Dependencies);
            missing |= result.MissingProjectReference;
        }
        var references = new List<MetadataReference>(Framework("Microsoft.NETCore.App", project.Target));
        if (project.AspNet || dependencies.Any(x => x.AspNet))
            references.AddRange(Framework("Microsoft.AspNetCore.App", project.Target));
        foreach (var target in dependencies.OrderBy(x => x.Path, StringComparer.Ordinal))
        {
            if (!projectReferences.TryGetValue(target, out var reference))
            {
                using var stream = new MemoryStream();
                var emission = builds[target].Compilation.Emit(stream,
                    options: new EmitOptions(metadataOnly: true, includePrivateMembers: false));
                if (emission.Success)
                {
                    if (stream.Length > 32 * 1024 * 1024 - emittedBytes) throw new OutputBudget();
                    emittedBytes += checked((int)stream.Length);
                    reference = MetadataReference.CreateFromImage(stream.ToArray());
                }
                projectReferences[target] = reference;
            }
            if (reference != null) references.Add(reference);
            else missing = true;
        }
        var compilation = project.VisualBasic ? VisualBasic(project, references) : CSharp(project, references);
        var build = new Build(compilation, dependencies.ToArray(), missing);
        visiting.Remove(project);
        builds.Add(project, build);
        return build;
    }

    private ImmutableArray<MetadataReference> Framework(string framework, string target)
    {
        var key = framework + "/" + target;
        if (frameworkReferences.TryGetValue(key, out var references)) return references;
        if (!capture.Runtime.References.TryGetValue(key, out var directory)) throw new UnsupportedProject();
        var paths = Directory.EnumerateFiles(directory, "*.dll").Take(1_025).Order(StringComparer.Ordinal).ToArray();
        if (paths.Length is 0 or > 1_024 || paths.Any(path => new FileInfo(path).LinkTarget != null))
            throw new UnsupportedProject();
        references = paths.Select(path => (MetadataReference)MetadataReference.CreateFromFile(path)).ToImmutableArray();
        frameworkReferences[key] = references;
        return references;
    }

    private static OutputKind Output(Project project)
        => project.Get("OutputType", project.Web && !project.VisualBasic ? "Exe" : "Library").ToLowerInvariant() switch
        {
            "library" => OutputKind.DynamicallyLinkedLibrary,
            "exe" => OutputKind.ConsoleApplication,
            "winexe" => OutputKind.WindowsApplication,
            _ => throw new UnsupportedProject(),
        };

    private static string[] Constants(Project project)
    {
        var version = project.Target[3..].Replace('.', '_');
        var target = "NET" + version;
        var numbers = version.Split('_').Select(int.Parse).ToArray();
        return ["DEBUG", "TRACE", "NET", "NETCOREAPP", target, target + "_OR_GREATER",
            .. Enumerable.Range(5, Math.Max(0, numbers[0] - 5)).Select(x => "NET" + x + "_0_OR_GREATER"),
            .. Enumerable.Range(0, 4).Select(x => "NETCOREAPP3_" + x + "_OR_GREATER")];
    }

    private static Compilation CSharp(Project project, IEnumerable<MetadataReference> references)
    {
        if (!CS.LanguageVersionFacts.TryParse(project.Get("LangVersion", "default"), out var version))
            throw new UnsupportedProject();
        var defines = project.Get("DefineConstants").Split([';', ',', ' '], StringSplitOptions.RemoveEmptyEntries);
        if (defines.Any(x => !CS.SyntaxFacts.IsValidIdentifier(x))) throw new UnsupportedProject();
        var parse = new CS.CSharpParseOptions(version, preprocessorSymbols: Constants(project).Concat(defines).Distinct());
        var trees = project.Sources.Select(file => CS.CSharpSyntaxTree.ParseText(
            SourceText.From(file.Text), parse, file.Unit)).ToList();
        List<string> imports = [.. project.Imports];
        if (project.Flag("ImplicitUsings"))
        {
            imports.AddRange(["System", "System.Collections.Generic", "System.IO", "System.Linq",
                "System.Net.Http", "System.Threading", "System.Threading.Tasks"]);
            if (project.Web) imports.AddRange(["System.Net.Http.Json", "Microsoft.AspNetCore.Builder",
                "Microsoft.AspNetCore.Hosting", "Microsoft.AspNetCore.Http", "Microsoft.AspNetCore.Routing",
                "Microsoft.Extensions.Configuration", "Microsoft.Extensions.DependencyInjection",
                "Microsoft.Extensions.Hosting", "Microsoft.Extensions.Logging"]);
        }
        if (imports.Any(x => !Regex.IsMatch(x, @"\A@?\w+(\.@?\w+)*\z"))) throw new UnsupportedProject();
        if (imports.Count != 0)
            trees.Add(CS.CSharpSyntaxTree.ParseText(
                string.Join('\n', imports.Distinct().Select(x => $"global using {x};")), parse));
        var nullable = project.Get("Nullable").ToLowerInvariant() switch
        {
            "" or "disable" => NullableContextOptions.Disable,
            "enable" => NullableContextOptions.Enable,
            "warnings" => NullableContextOptions.Warnings,
            "annotations" => NullableContextOptions.Annotations,
            _ => throw new UnsupportedProject(),
        };
        return CS.CSharpCompilation.Create(project.Assembly, trees, references,
            new CS.CSharpCompilationOptions(Output(project), allowUnsafe: project.Flag("AllowUnsafeBlocks"),
                nullableContextOptions: nullable));
    }

    private static Compilation VisualBasic(Project project, IEnumerable<MetadataReference> references)
    {
        var version = VB.LanguageVersion.Default;
        if (!VB.LanguageVersionFacts.TryParse(project.Get("LangVersion", "default"), ref version))
            throw new UnsupportedProject();
        Dictionary<string, object> constants = new(StringComparer.OrdinalIgnoreCase);
        foreach (var name in Constants(project)) constants[name] = true;
        constants["CONFIG"] = "Debug";
        constants["TARGET"] = Output(project) switch
        {
            OutputKind.ConsoleApplication => "exe",
            OutputKind.WindowsApplication => "winexe",
            _ => "library",
        };
        foreach (var definition in project.Get("DefineConstants")
            .Split([';', ','], StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries))
        {
            var pair = definition.Split('=', 2, StringSplitOptions.TrimEntries);
            if (!Regex.IsMatch(pair[0], @"\A\w+\z")) throw new UnsupportedProject();
            constants[pair[0]] = pair.Length == 1 ? true
                : bool.TryParse(pair[1], out var boolean) ? boolean
                : int.TryParse(pair[1], out var number) ? number
                : pair[1].Length >= 2 && pair[1][0] == '"' && pair[1][^1] == '"'
                ? pair[1][1..^1] : throw new UnsupportedProject();
        }
        var parse = new VB.VisualBasicParseOptions(version, preprocessorSymbols: constants);
        var trees = project.Sources.Select(file => VB.VisualBasicSyntaxTree.ParseText(
            SourceText.From(file.Text), parse, file.Unit));
        List<string> imports = [.. project.Imports];
        if (!project.Flag("DisableImplicitNamespaceImports"))
            imports.AddRange(["Microsoft.VisualBasic", "System", "System.Collections", "System.Collections.Generic",
                "System.Diagnostics", "System.Linq", "System.Xml.Linq", "System.Threading.Tasks"]);
        var compare = project.Get("OptionCompare", "binary").ToLowerInvariant();
        if (compare is not ("binary" or "text")) throw new UnsupportedProject();
        var strict = project.Get("OptionStrict").ToLowerInvariant() switch
        {
            "" or "off" => VB.OptionStrict.Off,
            "on" => VB.OptionStrict.On,
            "custom" => VB.OptionStrict.Custom,
            _ => throw new UnsupportedProject(),
        };
        try
        {
            return VB.VisualBasicCompilation.Create(project.Assembly, trees, references,
                new VB.VisualBasicCompilationOptions(Output(project),
                    rootNamespace: project.Get("RootNamespace", project.Loose ? "" :
                        Path.GetFileNameWithoutExtension(project.Path).Replace(' ', '_')),
                    globalImports: imports.Distinct(StringComparer.OrdinalIgnoreCase).Select(VB.GlobalImport.Parse),
                    optionStrict: strict, optionExplicit: project.Flag("OptionExplicit", true),
                    optionInfer: project.Flag("OptionInfer", true), optionCompareText: compare == "text"));
        }
        catch (ArgumentException)
        {
            throw new UnsupportedProject();
        }
    }
}
