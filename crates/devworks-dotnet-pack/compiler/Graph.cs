using System.Text;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.Text;
using CodeGraph.Extractors.CSharp;
using CodeGraph.Models;
using CS = Microsoft.CodeAnalysis.CSharp.Syntax;
using VB = Microsoft.CodeAnalysis.VisualBasic.Syntax;

namespace DevWorks.Dotnet;

internal sealed class Graph(Capture capture, Dictionary<AssemblyIdentity, string> projects)
{
    private readonly List<Unit> units = [];
    private readonly Dictionary<string, List<Target>> exact = new(StringComparer.Ordinal);

    internal sealed record Span(int StartLine, int StartColumn, int EndLine, int EndColumn, int StartByte, int EndByte);
    internal sealed record Node(string Kind, string Name, string QualifiedName, Span Span, int? Parent,
        Dictionary<string, object> Properties);
    private sealed record Edge(string Kind, int Source, object Target, string Resolution, double Confidence,
        Dictionary<string, object> Properties);
    internal sealed record Target(Unit Unit, int Index);
    private sealed record Relation(string Kind, int From, string Key, bool Exact, double Confidence,
        Dictionary<string, object> Properties);

    internal sealed class Unit
    {
        internal readonly CapturedFile File;
        internal readonly Project? Project;
        internal readonly List<Node> Nodes = [];
        private readonly List<Edge> edges = [];
        private readonly List<Relation> relations = [];
        internal readonly Dictionary<ISymbol, int> Symbols = new(SymbolEqualityComparer.Default);
        internal readonly Dictionary<string, List<int>> Aliases = new(StringComparer.Ordinal);
        private readonly int[] offsets, lineStarts;

        internal Unit(CapturedFile file, Project? project, Runtime runtime, string? gap)
        {
            File = file;
            Project = project;
            offsets = new int[file.Text.Length + 1];
            List<int> lines = [0];
            var total = 0;
            for (var i = 0; i < file.Text.Length;)
            {
                offsets[i] = total;
                var width = char.IsSurrogatePair(file.Text, i) ? 2 : 1;
                if (width == 2) offsets[i + 1] = total;
                total += Encoding.UTF8.GetByteCount(file.Text.AsSpan(i, width));
                if (file.Text[i] == '\n') lines.Add(total);
                i += width;
            }
            offsets[^1] = total;
            lineStarts = lines.ToArray();
            var properties = Properties();
            properties["sha256"] = file.Sha256;
            properties["sdkVersion"] = runtime.SdkVersion;
            properties["runtimeVersion"] = runtime.RuntimeVersion;
            if (gap != null) properties["coverageGap"] = gap;
            Nodes.Add(new("File", Path.GetFileName(file.Unit), file.Unit,
                Range(new(0, file.Text.Length)), null, properties));
        }

        internal Dictionary<string, object> Properties(string basis = "roslyn")
        {
            Dictionary<string, object> info = new()
            {
                ["semanticBasis"] = basis,
                ["language"] = File.Language,
            };
            if (Project != null)
            {
                info["projectPath"] = Project.Path;
                info["assembly"] = Project.Assembly;
                info["targetFramework"] = Project.Target;
            }
            return info;
        }

        internal Span Range(TextSpan span)
        {
            var (startLine, startCol) = Position(offsets[span.Start]);
            var (endLine, endCol) = Position(offsets[span.End]);
            return new(startLine, startCol, endLine, endCol, offsets[span.Start], offsets[span.End]);
        }

        private (int Line, int Column) Position(int offset)
        {
            var line = Array.BinarySearch(lineStarts, offset);
            if (line < 0) line = ~line - 1;
            return (line + 1, offset - lineStarts[line] + 1);
        }

        internal int Parent(ISymbol? symbol)
        {
            for (var current = symbol; current != null; current = current.ContainingSymbol)
                if (Symbols.TryGetValue(current, out var index)) return index;
            return 0;
        }

        internal int Add(Node node, params string[] names)
        {
            var i = Nodes.Count;
            Nodes.Add(node);
            foreach (var name in names.Distinct(StringComparer.Ordinal))
            {
                if (!Aliases.TryGetValue(name, out var indices)) Aliases[name] = indices = [];
                indices.Add(i);
            }
            return i;
        }

        internal void Link(string kind, int source, string key, bool exact, double confidence,
            Dictionary<string, object> props)
        {
            if (key.Length is > 0 and <= 4096) relations.Add(new(kind, source, key, exact, confidence, props));
        }

        internal void Local(string kind, int source, int destination, Dictionary<string, object> props)
            => edges.Add(new(kind, source, new { local = destination }, "resolved", 1, props));

        private void Finish(Dictionary<string, List<Target>> index)
        {
            foreach (var rel in relations)
            {
                var to = rel.Exact && index.TryGetValue(rel.Key, out var targets) ? targets[0] : null;
                if (!rel.Exact && !rel.Properties.ContainsKey("compiler_unresolved")
                    && Aliases.TryGetValue(rel.Key, out var candidates) && candidates.Count == 1)
                    to = new(this, candidates[0]);
                var node = to?.Unit.Nodes[to.Index];
                object target;
                var resolution = "unresolved";
                if (to != null && node != null)
                {
                    target = to.Unit == this ? new { local = to.Index }
                        : new { node = new { unit = to.Unit.File.Unit, kind = node.Kind,
                            qualified_name = node.QualifiedName } };
                    resolution = rel.Exact ? "resolved" : "inferred";
                }
                else
                {
                    target = new { symbol = rel.Key };
                    rel.Properties["compiler_unresolved"] = true;
                }
                edges.Add(new(rel.Kind, rel.From, target, resolution, rel.Confidence, rel.Properties));
            }
        }

        internal object Result(Dictionary<string, List<Target>> index)
        {
            Finish(index);
            return new { unitKey = File.Unit, nodes = Nodes, edges };
        }
    }

    internal void Exclude(CapturedFile file)
        => units.Add(new Unit(file, null, capture.Runtime, "dotnet-source-excluded"));

    internal void Add(CapturedFile file, Project project, SemanticModel model, string? gap)
    {
        var unit = new Unit(file, project, capture.Runtime, gap);
        units.Add(unit);
        var root = model.SyntaxTree.GetRoot();
        foreach (var syntax in root.DescendantNodes())
        {
            if (!IsDeclaration(syntax)) continue;
            var symbol = model.GetDeclaredSymbol(syntax);
            var kind = Kind(symbol);
            if (kind == null || symbol == null || symbol.IsImplicitlyDeclared) continue;
            var display = symbol.ToDisplayString(SymbolDisplayFormat.CSharpErrorMessageFormat);
            var qualified = Key(symbol);
            if (qualified == null) continue;

            var parent = unit.Parent(symbol.ContainingSymbol);
            var info = unit.Properties();
            info["symbol"] = display;
            info["documentationId"] = symbol.GetDocumentationCommentId() ?? qualified;
            info["accessibility"] = symbol.DeclaredAccessibility.ToString();
            if (symbol is IMethodSymbol method)
            {
                info["parameters"] = method.Parameters.Select(p => p.Type.ToDisplayString()).ToArray();
                info["return_type"] = method.ReturnType.ToDisplayString();
                info["signature"] = display;
            }
            var declaration = Extent(syntax);
            var name = symbol is IMethodSymbol { MethodKind: MethodKind.Constructor }
                ? symbol.ContainingType.Name : symbol.Name;
            if (string.IsNullOrEmpty(name)) continue;
            var simple = LegacyName(symbol);
            var legacy = simple == display ? new[] { display, qualified } : new[] { display, qualified, simple };
            var i = unit.Add(new(kind, name, qualified, unit.Range(declaration), parent, info), legacy);
            unit.Symbols[symbol] = i;
            if (!exact.TryGetValue(qualified, out var targets)) exact[qualified] = targets = [];
            targets.Add(new(unit, i));
            unit.Local(symbol is IMethodSymbol ? "DEFINES_METHOD" : "DEFINES", parent, i, unit.Properties());
            if (symbol is INamedTypeSymbol type)
            {
                if (type.BaseType is { SpecialType: SpecialType.None } baseType)
                    Link(unit, i, baseType, "INHERITS", declaration);
                foreach (var iface in type.Interfaces)
                    Link(unit, i, iface, type.TypeKind == TypeKind.Interface ? "INHERITS" : "IMPLEMENTS", declaration);
            }
            if (project.VisualBasic) VisualBasicFacts.Declaration(this, unit, model, syntax, symbol, i);
        }
        foreach (var syntax in root.DescendantNodes())
        {
            if (IsInvocation(syntax))
            {
                var symbol = model.GetSymbolInfo(syntax).Symbol as IMethodSymbol;
                var parent = unit.Parent(model.GetEnclosingSymbol(syntax.SpanStart));
                if (symbol != null)
                    Link(unit, parent, symbol, "CALLS", syntax.Span);
                else
                {
                    var info = unit.Properties("syntax");
                    var candidate = syntax switch
                    {
                        CS.InvocationExpressionSyntax { Expression: CS.MemberAccessExpressionSyntax member } => member.Name.Identifier.ValueText,
                        CS.InvocationExpressionSyntax { Expression: CS.SimpleNameSyntax name } => name.Identifier.ValueText,
                        VB.InvocationExpressionSyntax { Expression: VB.MemberAccessExpressionSyntax member } => member.Name.Identifier.ValueText,
                        VB.InvocationExpressionSyntax { Expression: VB.SimpleNameSyntax name } => name.Identifier.ValueText,
                        _ => syntax.ToString(),
                    };
                    info["compilerCandidateReason"] = model.GetSymbolInfo(syntax).CandidateReason.ToString();
                    Hint(unit, parent, "CALLS", candidate, syntax.Span, 0.5, info);
                }
                if (project.VisualBasic)
                    VisualBasicFacts.Invocation(this, unit, model, syntax, symbol, parent);
            }
            else if (syntax is CS.UsingDirectiveSyntax or VB.SimpleImportsClauseSyntax or VB.HandlesClauseItemSyntax)
            {
                SyntaxNode? target = syntax switch
                {
                    CS.UsingDirectiveSyntax imports => imports.NamespaceOrType,
                    VB.SimpleImportsClauseSyntax imports => imports.Name,
                    VB.HandlesClauseItemSyntax handle => handle.EventMember,
                    _ => null,
                };
                if (target == null) continue;
                var kind = syntax is VB.HandlesClauseItemSyntax ? "HANDLES" : "IMPORTS";
                var parent = unit.Parent(model.GetEnclosingSymbol(syntax.SpanStart));
                if (model.GetSymbolInfo(target).Symbol is { } symbol) Link(unit, parent, symbol, kind, syntax.Span);
                else Hint(unit, parent, kind, target.ToString(), syntax.Span, 0.5);
            }
            else if (syntax is CS.IdentifierNameSyntax or VB.IdentifierNameSyntax)
            {
                if (model.GetSymbolInfo(syntax).Symbol is { } binding
                    && binding is IFieldSymbol or IPropertySymbol or INamedTypeSymbol)
                    Link(unit, unit.Parent(model.GetEnclosingSymbol(syntax.SpanStart)), binding, "REFERENCES", syntax.Span);
            }
        }
        if (!project.VisualBasic) RetainCSharp(unit, model, root);
    }

    private static bool IsDeclaration(SyntaxNode node) => node is
        CS.BaseNamespaceDeclarationSyntax or CS.BaseTypeDeclarationSyntax or CS.DelegateDeclarationSyntax
        or CS.BaseMethodDeclarationSyntax or CS.LocalFunctionStatementSyntax or CS.BasePropertyDeclarationSyntax
        or CS.VariableDeclaratorSyntax or CS.EnumMemberDeclarationSyntax
        or VB.NamespaceStatementSyntax or VB.TypeStatementSyntax or VB.EnumStatementSyntax
        or VB.DelegateStatementSyntax or VB.MethodStatementSyntax or VB.SubNewStatementSyntax
        or VB.OperatorStatementSyntax or VB.PropertyStatementSyntax or VB.EventStatementSyntax
        or VB.ModifiedIdentifierSyntax or VB.EnumMemberDeclarationSyntax;

    private static bool IsInvocation(SyntaxNode node) => node is
        CS.InvocationExpressionSyntax or CS.BaseObjectCreationExpressionSyntax or CS.ConstructorInitializerSyntax
        or VB.InvocationExpressionSyntax or VB.ObjectCreationExpressionSyntax;

    private static TextSpan Extent(SyntaxNode node) => node.Parent is
        VB.TypeBlockSyntax or VB.NamespaceBlockSyntax or VB.EnumBlockSyntax
        or VB.MethodBlockBaseSyntax or VB.PropertyBlockSyntax or VB.EventBlockSyntax
        ? node.Parent.Span : node.Span;

    private static string? Kind(ISymbol? symbol) => symbol switch
    {
        INamespaceSymbol => "Namespace",
        INamedTypeSymbol { IsRecord: true } => "Record",
        INamedTypeSymbol { TypeKind: TypeKind.Class } => "Class",
        INamedTypeSymbol { TypeKind: TypeKind.Interface } => "Interface",
        INamedTypeSymbol { TypeKind: TypeKind.Struct } => "Struct",
        INamedTypeSymbol { TypeKind: TypeKind.Enum } => "Enum",
        INamedTypeSymbol { TypeKind: TypeKind.Delegate } => "Delegate",
        INamedTypeSymbol { TypeKind: TypeKind.Module } => "Module",
        IMethodSymbol => "Method",
        IPropertySymbol => "Property",
        IFieldSymbol => "Field",
        IEventSymbol => "Event",
        _ => null,
    };

    private static string LegacyName(ISymbol symbol)
        => symbol is IMethodSymbol method
            ? $"{method.ContainingType?.ToDisplayString()}.{method.Name}"
            : symbol.ToDisplayString();

    internal string? Key(ISymbol symbol)
    {
        if (symbol is IMethodSymbol { ReducedFrom: { } definition }) symbol = definition;
        symbol = symbol.OriginalDefinition;
        var assembly = symbol.ContainingAssembly?.Identity;
        if (assembly == null || !projects.TryGetValue(assembly, out var project)) return null;
        if (symbol is IMethodSymbol { MethodKind: MethodKind.LocalFunction }
            && symbol.Locations.FirstOrDefault(x => x.IsInSource) is { } source)
            return $"dotnet:{project}:{source.SourceTree!.FilePath}@{source.SourceSpan.Start}:{symbol.Name}";
        return symbol.GetDocumentationCommentId() is { } name ? $"dotnet:{project}:{name}" : null;
    }

    internal void Link(Unit unit, int source, ISymbol symbol, string kind, TextSpan span,
        Dictionary<string, object>? info = null)
    {
        info ??= unit.Properties();
        info["useSpan"] = unit.Range(span);
        var key = Key(symbol);
        if (key != null)
            unit.Link(kind, source, key, true, 1, info);
        else
        {
            info["compilerBoundExternal"] = true;
            info["compiler_unresolved"] = true;
            unit.Link(kind, source, symbol.ToDisplayString(), false, 1, info);
        }
    }

    internal static void Hint(Unit unit, int source, string kind, string target, TextSpan range, double confidence,
        Dictionary<string, object>? properties = null)
    {
        properties ??= unit.Properties("syntax");
        properties["useSpan"] = unit.Range(range);
        properties["compiler_unresolved"] = true;
        unit.Link(kind, source, target, false, confidence, properties);
    }

    private void RetainCSharp(Unit unit, SemanticModel model, SyntaxNode root)
    {
        var walk = new CodeGraphSyntaxWalker(new ExtractorContext
        {
            RepositoryName = capture.RepositoryId,
            RootPath = "",
            DotnetProject = unit.Project!.Assembly,
        }, model);
        walk.Visit(root);
        var result = walk.GetResult();
        var text = model.SyntaxTree.GetText();
        foreach (var node in result.Nodes)
        {
            var from = text.Lines[Math.Clamp(node.StartLine - 1, 0, text.Lines.Count - 1)].Start;
            var to = text.Lines[Math.Clamp(Math.Max(node.EndLine, node.StartLine) - 1,
                0, text.Lines.Count - 1)].End;
            var candidates = unit.Aliases.GetValueOrDefault(node.QualifiedName, [])
                .Where(i => unit.Nodes[i].Kind == node.Label.ToString()
                    && unit.Nodes[i].Span.StartLine == node.StartLine).ToArray();
            if (candidates.Length == 1)
            {
                foreach (var (key, value) in node.Properties) unit.Nodes[candidates[0]].Properties.TryAdd(key, value);
            }
            else if (candidates.Length == 0 && node.Label is NodeLabel.Route or NodeLabel.Queue
                or NodeLabel.Exchange or NodeLabel.Service)
            {
                var info = unit.Properties("retained-csharp");
                foreach (var (key, value) in node.Properties) info[key] = value;
                var qualified = $"dotnet:{unit.Project!.Path}:{node.QualifiedName}";
                unit.Add(new(node.Label.ToString(), node.Name, qualified, unit.Range(TextSpan.FromBounds(from, to)),
                    0, info), node.QualifiedName);
            }
        }
        foreach (var edge in result.Edges.Where(e => e.Type is not (EdgeType.CALLS or EdgeType.DEFINES_METHOD)))
        {
            var from = unit.Aliases.GetValueOrDefault(edge.SourceQN, []);
            if (from.Count == 0) continue;
            var info = unit.Properties("retained-csharp");
            if (edge.Properties != null)
                foreach (var (key, val) in edge.Properties) info[key] = val;
            info["retainedSource"] = edge.SourceQN;
            if (from.Count != 1) info["ambiguousSource"] = true;
            unit.Link(edge.Type.ToString(), from.Count == 1 ? from[0] : 0, edge.TargetQN, false,
                info.GetValueOrDefault("confidence_band") is "high"
                    ? 0.9 : 0.65, info);
        }
        foreach (var call in result.UnresolvedCalls)
        {
            var from = unit.Aliases.GetValueOrDefault(call.CallerQN, []);
            var info = unit.Properties("retained-csharp");
            info["compiler_unresolved"] = true;
            info["receiverKind"] = call.ReceiverKind.ToString();
            info["retainedSource"] = call.CallerQN;
            if (call.ReceiverType != null) info["receiverType"] = call.ReceiverType;
            if (call.ArgumentLabels != null) info["argumentLabels"] = call.ArgumentLabels;
            unit.Link("CALLS", from.Count == 1 ? from[0] : 0, call.CalleeName, false, call.Confidence, info);
        }
        foreach (var import in result.UnresolvedImports)
            unit.Link("IMPORTS", 0, import.ImportedNamespace, false, 0.65, unit.Properties("retained-csharp"));
    }

    internal object[] Result()
    {
        foreach (var targets in exact.Values)
            targets.Sort((left, right) => StringComparer.Ordinal.Compare(left.Unit.File.Unit, right.Unit.File.Unit));
        return units.OrderBy(x => x.File.Unit, StringComparer.Ordinal).Select(x => x.Result(exact)).ToArray();
    }
}
