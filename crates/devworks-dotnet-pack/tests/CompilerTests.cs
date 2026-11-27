using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;

namespace DevWorks.Dotnet;

public class CompilerTests
{
    private static CapturedFile File(string name, string text) => new(name,
        name.EndsWith(".cs") ? "csharp" : name.EndsWith(".vb") ? "visual-basic" : "dotnet-context", "fixture", text);

    private static Capture Snapshot(params CapturedFile[] files)
    {
        var runtime = new DirectoryInfo(RuntimeEnvironment.GetRuntimeDirectory());
        var root = runtime.Parent!.Parent!.Parent!.FullName;
        var version = Environment.Version.ToString(3);
        return new("fixture", new string('a', 40), null, files, new("10.0.202", version,
            runtime.FullName, new[] { "Microsoft.NETCore.App", "Microsoft.AspNetCore.App" }.ToDictionary(
                framework => framework + "/net10.0",
                framework => Path.Combine(root, "packs", framework + ".Ref", version, "ref", "net10.0"))));
    }

    private static JsonElement Index(Capture capture)
        => JsonSerializer.SerializeToElement(new Workspace(capture).Index(), Program.Json);
    private static JsonElement[] Items(JsonElement unit, string list)
        => unit.GetProperty(list).EnumerateArray().ToArray();
    private static string Str(JsonElement row, string field) => row.GetProperty(field).GetString()!;
    private static JsonElement Props(JsonElement element) => element.GetProperty("properties");

    [Fact]
    public void VisualBasicUsesRealCompilerSymbolsAndFrameworkFacts()
    {
        const string source = """
            ' café 🚀
            Imports System
            Imports System.Net.Http
            Imports Microsoft.AspNetCore.Mvc
            Imports Fixture.Shared
            Imports SharedName = Fixture.Shared.Values

            Namespace ConsumerApp
                Public Class FactAttribute
                    Inherits Attribute
                End Class
                Public Interface IWork
                    Sub Handle()
                End Interface
                Public Class Work
                    Implements IWork
                    Public Sub Handle() Implements IWork.Handle
                    End Sub
                End Class
                Public Class EntityNameAttribute
                    Inherits Attribute
                    Public Sub New(queue As String, exchange As String)
                    End Sub
                End Class
                <EntityName("orders", "work")>
                Public Class OrderMessage
                    Public Property Item As Work
                End Class
                Public Interface IConsumer(Of T)
                End Interface
                Public Interface IPublishEndpoint
                    Sub Publish(Of T)(value As T)
                End Interface
                Public Class Registry
                    Public Sub AddScoped(Of T, U)()
                    End Sub
                    Public Sub AddConsumer(Of T)()
                    End Sub
                    Public Sub Consumer(Of T)()
                    End Sub
                    Public Sub ReceiveEndpoint(queue As String, setup As Action(Of Registry))
                    End Sub
                End Class
                Public Class Base
                End Class
                <Route("api/[controller]")>
                Public Class EntryController
                    Inherits Base
                    Implements IConsumer(Of OrderMessage)
                    Public Event Received()
                    Public Sub New(work As IWork)
                    End Sub
                    <Fact, HttpGet("items")>
                    Public Function Run(bus As IPublishEndpoint, registry As Registry, client As HttpClient) As Integer
                        registry.AddScoped(Of IWork, Work)()
                        registry.AddConsumer(Of EntryController)()
                        registry.ReceiveEndpoint("orders", Sub(e) e.Consumer(Of EntryController)())
                        bus.Publish(Of OrderMessage)(New OrderMessage())
                        Dim response = client.GetAsync("/api/status")
                        Dim absent As MissingThing
                        absent.Handle()
                        Dim bumped = 42.Bump()
                        If response IsNot Nothing AndAlso True Then
                            Return SharedName.Answer(42)
                        ElseIf False Then
                            Return 0
                        End If
                        Return If(True, 1, 2)
                    End Function
                    Private Sub OnReceived() Handles Me.Received
                    End Sub
                End Class
                Public Module Extra
                End Module
                Public Structure Value
                End Structure
                Public Enum State
                    Yes
                End Enum
                Public Delegate Sub Done()
            End Namespace
            """;
        var index = Index(Snapshot(
            File("shared/Shared.csproj", """<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup></Project>"""),
            File("shared/Values.cs", """
                namespace Fixture.Shared;
                public static class Extensions {
                    public static int Bump(this int value) => value + 1;
                }
                public static class Values {
                    public static int Answer(int value) => value;
                    public static string Answer(string value) => value;
                }
                """),
            File("app/App.vbproj", """
                <Project Sdk="Microsoft.NET.Sdk">
                  <PropertyGroup><RootNamespace></RootNamespace><TargetFramework>net10.0</TargetFramework></PropertyGroup>
                  <ItemGroup><FrameworkReference Include="Microsoft.AspNetCore.App"/><ProjectReference Include="../shared/Shared.csproj"/></ItemGroup>
                </Project>
                """),
            File("app/Entry.vb", source)));
        var unit = index.EnumerateArray().Single(u => Str(u, "unitKey") == "app/Entry.vb");
        var nodes = Items(unit, "nodes");
        var edges = Items(unit, "edges");
        foreach (var kind in new[] { "Namespace", "Class", "Interface", "Property", "Module", "Struct",
                     "Enum", "Delegate", "Route", "Queue", "Exchange", "Service", "Event" })
            Assert.Contains(nodes, node => Str(node, "kind") == kind);
        foreach (var kind in new[] { "DEFINES_METHOD", "INHERITS", "IMPLEMENTS", "INJECTS", "CONSUMES",
                     "PUBLISHES", "REGISTERS", "ROUTED_TO", "BOUND_TO", "CARRIES_FIELD", "IMPORTS",
                     "CALLS", "HTTP_CALLS", "HANDLES" })
            Assert.Contains(edges, edge => Str(edge, "kind") == kind);
        var method = nodes.Single(node => Str(node, "name") == "Run");
        Assert.True(Props(method).GetProperty("is_test").GetBoolean());
        Assert.Equal(5, Props(method).GetProperty("complexity").GetInt32());
        var span = method.GetProperty("span");
        var bytes = Encoding.UTF8.GetBytes(source);
        Assert.StartsWith("<Fact, HttpGet", Encoding.UTF8.GetString(bytes, span.GetProperty("startByte").GetInt32(), 24));
        var target = edges.Where(edge => Str(edge, "kind") == "CALLS")
            .Select(edge => edge.GetProperty("target"))
            .Where(target => target.TryGetProperty("node", out _))
            .Select(target => target.GetProperty("node"))
            .Single(target => Str(target, "unit") == "shared/Values.cs"
                && Str(target, "qualified_name").Contains(".Answer("));
        Assert.EndsWith("M:Fixture.Shared.Values.Answer(System.Int32)", Str(target, "qualified_name"));
        Assert.Contains(edges, edge => Str(edge, "kind") == "CALLS" && Str(edge, "resolution") == "resolved"
            && edge.GetProperty("target").TryGetProperty("node", out var extension)
            && Str(extension, "qualified_name").EndsWith("M:Fixture.Shared.Extensions.Bump(System.Int32)"));
        Assert.Contains(edges, edge => Str(edge, "kind") == "CALLS" && Str(edge, "resolution") == "resolved"
            && edge.GetProperty("target").TryGetProperty("local", out var ctor)
            && Str(Props(nodes[ctor.GetInt32()]), "documentationId").EndsWith(".OrderMessage.#ctor"));
        Assert.Contains(edges, edge => Str(edge, "kind") == "IMPLEMENTS" && Str(edge, "resolution") == "resolved"
            && edge.GetProperty("target").TryGetProperty("local", out var impl)
            && Str(Props(nodes[impl.GetInt32()]), "documentationId").EndsWith(".IWork.Handle"));
        Assert.Contains(edges, edge => Str(edge, "kind") == "CALLS" && Str(edge, "resolution") == "unresolved"
            && edge.GetProperty("confidence").GetDouble() < 1 && Str(edge.GetProperty("target"), "symbol") == "Handle");
        var service = Props(nodes.Single(node => Str(node, "kind") == "Service"));
        Assert.Equal("Scoped", Str(service, "lifetime"));
        Assert.EndsWith(".IWork", Str(service, "interface"));
        Assert.EndsWith(".Work", Str(service, "implementation"));
        Assert.Contains(edges, edge => Str(edge, "kind") == "HTTP_CALLS"
            && Str(Props(edge), "url_pattern") == "/api/status");
        Assert.Contains(edges, edge => Str(edge, "kind") == "REGISTERS"
            && Props(edge).TryGetProperty("queue_name", out var queue) && queue.GetString() == "orders");
    }

    [Fact]
    public void UnavailablePackagesKeepLocalBindingAndDoNotGuessTargets()
    {
        var unit = Index(Snapshot(
            File("Sample.csproj", """
                <Project Sdk="Microsoft.NET.Sdk">
                  <PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>
                  <ItemGroup><PackageReference Include="Unavailable" Version="1.0.0"/></ItemGroup>
                </Project>
                """),
            File("Code.cs", """
                public class Worker {
                   int Hit() => 1;
                   int Run() => Hit();
                   void Relay(External outside) { outside.Hit(); }
                }
                """)))[0];
        Assert.Equal("dotnet-external-assemblies-unavailable", Str(Props(Items(unit, "nodes")[0]), "coverageGap"));
        var calls = Items(unit, "edges").Where(edge => Str(edge, "kind") == "CALLS").ToList();
        Assert.Contains(calls, edge => Str(edge, "resolution") == "resolved");
        Assert.Contains(calls, edge => Str(edge, "resolution") == "unresolved"
            && Props(edge).GetProperty("compiler_unresolved").GetBoolean());
    }

    [Fact]
    public void RetainedCSharpFactsReachTheProjectedGraph()
    {
        var source = """
            using Microsoft.AspNetCore.Mvc;
            #if NETCOREAPP1_0_OR_GREATER && !NETCOREAPP3_2_OR_GREATER
            namespace Fixture;
            public class FactAttribute : System.Attribute {}
            public interface IConsumer<T> {}
            public interface IFoo {}
            public class Foo : IFoo {}
            public record OrderMessage(string Name);
            public class Registry {
                public void AddScoped<I, T>() {}
            }
            [Route("api/items")]
            public class ItemsController : IConsumer<OrderMessage> {
                [Fact, HttpGet("details")]
                public string Query(Registry registry, bool next) {
                    registry.AddScoped<IFoo, Foo>();
                    return new OrderMessage(next ? "one" : "two").Name;
                }
            }
            #endif
            """.Replace("\n", "\r");
        var unit = Index(Snapshot(
            File("App.csproj", """
                <Project Sdk="Microsoft.NET.Sdk">
                  <PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>
                  <ItemGroup><FrameworkReference Include="Microsoft.AspNetCore.App"/></ItemGroup>
                </Project>
                """),
            File("Code.cs", source)))[0];
        var nodes = Items(unit, "nodes");
        var edges = Items(unit, "edges");
        var method = Props(nodes.Single(node => Str(node, "name") == "Query"));
        Assert.True(method.GetProperty("is_test").GetBoolean());
        Assert.Equal(2, method.GetProperty("complexity").GetInt32());
        Assert.Contains(nodes, node => Str(node, "kind") == "Route"
            && Str(Props(node), "route_template").Contains("details"));
        Assert.Contains(nodes, node => Str(node, "kind") == "Service"
            && Str(Props(node), "lifetime") == "Scoped");
        Assert.Contains(edges, edge => Str(edge, "kind") == "HANDLES");
        Assert.Contains(edges, edge => Str(edge, "kind") == "CONSUMES");
        Assert.Contains(edges, edge => Str(edge, "kind") == "REFERENCES" && Str(edge, "resolution") == "resolved"
            && edge.GetProperty("target").TryGetProperty("local", out var target)
            && Str(Props(nodes[target.GetInt32()]), "documentationId") == "P:Fixture.OrderMessage.Name");
    }

    [Theory]
    [InlineData("<Target Name='Vetted' BeforeTargets='Build'><Error Text='canary'/></Target>")]
    [InlineData("<ItemGroup><Analyzer Include='generator.dll'/></ItemGroup>")]
    [InlineData("<PropertyGroup Condition='true'><DefineConstants>HIDDEN</DefineConstants></PropertyGroup>")]
    [InlineData("<Import Project='outside.props'/>")]
    public void RepositoryBuildExtensionsAreRejected(string element)
    {
        var project = "<Project Sdk='Microsoft.NET.Sdk'><PropertyGroup><TargetFramework>net10.0</TargetFramework></PropertyGroup>"
            + element + "</Project>";
        Assert.Throws<UnsupportedProject>(() => Index(Snapshot(File("App.csproj", project),
            File("Code.cs", "public class App {}"))));
    }
}
