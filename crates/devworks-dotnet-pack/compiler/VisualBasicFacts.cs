using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.Operations;
using VB = Microsoft.CodeAnalysis.VisualBasic;
using V = Microsoft.CodeAnalysis.VisualBasic.Syntax;

namespace DevWorks.Dotnet;

internal static class VisualBasicFacts
{
    internal static void Declaration(Graph graph, Graph.Unit unit, SemanticModel model,
        SyntaxNode syntax, ISymbol symbol, int index)
    {
        var props = unit.Nodes[index].Properties;
        if (symbol is IMethodSymbol method)
        {
            var body = syntax.Parent is V.MethodBlockBaseSyntax ? syntax.Parent : syntax;
            props["complexity"] = 1 + body.DescendantNodes().Count(Branch);
            if (method.GetAttributes().Any(a => Name(a) is "Fact" or "Theory" or "Test" or "TestMethod"))
                props["is_test"] = true;
            if (method.MethodKind == MethodKind.Constructor)
            {
                foreach (var parameter in method.Parameters)
                    if (IsDomain(parameter.Type))
                        graph.Link(unit, index, parameter.Type, "INJECTS", syntax.Span);
            }
            foreach (var implementation in method.ExplicitInterfaceImplementations)
                graph.Link(unit, index, implementation, "IMPLEMENTS", syntax.Span);
            foreach (var attribute in method.GetAttributes())
            {
                var verb = Name(attribute) switch
                {
                    "HttpGet" => "GET", "HttpPost" => "POST", "HttpPut" => "PUT",
                    "HttpDelete" => "DELETE", "HttpPatch" => "PATCH", "Route" => "ANY", _ => null,
                };
                if (verb == null) continue;
                var template = Argument(attribute, 0) ?? "";
                var prefix = method.ContainingType.GetAttributes()
                    .Where(a => Name(a) == "Route").Select(a => Argument(a, 0)).FirstOrDefault() ?? "";
                var path = template.StartsWith('/') ? template :
                    "/" + string.Join('/', new[] { prefix.Trim('/'), template.Trim('/') }.Where(x => x.Length > 0));
                path = path.Replace("[controller]", method.ContainingType.Name
                    .Replace("Controller", "", StringComparison.OrdinalIgnoreCase), StringComparison.OrdinalIgnoreCase)
                    .Replace("[action]", method.Name, StringComparison.OrdinalIgnoreCase);
                var route = Synthetic(unit, "Route", verb + " " + path, index, syntax,
                    new() { ["http_method"] = verb, ["route_template"] = path });
                graph.Link(unit, route, method, "HANDLES", syntax.Span);
            }
        }
        if (symbol is not INamedTypeSymbol type) return;
        var consumers = type.AllInterfaces.Concat(BaseTypes(type))
            .Where(t => t.Name is "Consumer" or "IConsumer" or "TcConsumer" or "ITcConsumer")
            .Where(t => t.TypeArguments.Length == 1).ToArray();
        foreach (var consumer in consumers)
            graph.Link(unit, index, consumer.TypeArguments[0], "CONSUMES", syntax.Span);
        if (consumers.Length != 0)
        {
            foreach (var attribute in type.GetAttributes())
            {
                var name = Name(attribute);
                if (name is "ConcurrencyLimit" or "PrefetchCount" or "RetryPolicy"
                    && attribute.ConstructorArguments.FirstOrDefault().Value is { } value)
                    props[name switch
                    {
                        "ConcurrencyLimit" => "concurrency_limit",
                        "PrefetchCount" => "prefetch_count",
                        _ => "retry_policy",
                    }] = value;
                if (name != "RetryPolicy") continue;
                foreach (var pair in attribute.NamedArguments)
                {
                    if (pair.Value.Value == null) continue;
                    if (pair.Key is "RetryCount" or "Retries") props["retry_count"] = pair.Value.Value;
                    if (pair.Key is "Interval" or "RetryInterval") props["retry_interval"] = pair.Value.Value;
                }
            }
        }
        var routeAttribute = type.GetAttributes().FirstOrDefault(a =>
            Name(a) is "EntityName" or "MessageUrn" or "QueueName" or "ExchangeName" or "Endpoint");
        if (routeAttribute != null)
        {
            props["is_message_contract"] = true;
            var queue = Named(routeAttribute, "QueueName", "Queue") ?? Argument(routeAttribute, 0);
            var exchange = Named(routeAttribute, "ExchangeName", "Exchange") ?? Argument(routeAttribute, 1);
            var host = Named(routeAttribute, "VirtualHost") ?? Argument(routeAttribute, 2);
            var routing = Named(routeAttribute, "RoutingKey");
            if (queue != null) props["queue_name"] = queue;
            if (exchange != null) props["exchange_name"] = exchange;
            if (host != null) props["virtual_host"] = host;
            if (routing != null) props["routing_key"] = routing;
            if (!string.IsNullOrEmpty(queue))
            {
                var queueIndex = Synthetic(unit, "Queue", queue, index, syntax,
                    new() { ["queue_name"] = queue, ["virtual_host"] = host ?? "/" });
                unit.Local("ROUTED_TO", index, queueIndex, unit.Properties());
                if (!string.IsNullOrEmpty(exchange))
                {
                    var exchangeIndex = Synthetic(unit, "Exchange", exchange, index, syntax,
                        new() { ["exchange_name"] = exchange, ["virtual_host"] = host ?? "/" });
                    unit.Local("BOUND_TO", queueIndex, exchangeIndex, unit.Properties());
                }
            }
        }
        if (routeAttribute != null || new[] { "Event", "Command", "Message" }.Any(s => type.Name.EndsWith(s, StringComparison.Ordinal)))
        {
            var fields = type.GetMembers().OfType<IPropertySymbol>()
                .Where(p => p.DeclaredAccessibility == Accessibility.Public && !p.IsStatic).ToArray();
            props["fields"] = fields.Select(p => new { name = p.Name, type = p.Type.ToDisplayString() }).ToArray();
            foreach (var field in fields)
            {
                var domain = field.Type switch
                {
                    IArrayTypeSymbol array => array.ElementType,
                    INamedTypeSymbol { TypeArguments.Length: 1 } generic
                        when generic.AllInterfaces.Any(i => i.Name == "IEnumerable") => generic.TypeArguments[0],
                    _ => field.Type,
                };
                if (IsDomain(domain))
                {
                    var info = unit.Properties();
                    info["field_name"] = field.Name;
                    graph.Link(unit, index, domain, "CARRIES_FIELD", syntax.Span, info);
                }
            }
        }
    }

    internal static void Invocation(Graph graph, Graph.Unit unit, SemanticModel model,
        SyntaxNode syntax, IMethodSymbol? method, int source)
    {
        if (method == null || model.GetOperation(syntax) is not IInvocationOperation invocation) return;
        var owner = method.ContainingType.ToDisplayString();
        var arg = invocation.Arguments.FirstOrDefault(a => !a.IsImplicit)?.Value;
        if (owner.Contains("HttpClient", StringComparison.Ordinal))
        {
            var verb = method.Name switch
            {
                "GetAsync" or "GetFromJsonAsync" or "GetStringAsync" or "GetStreamAsync" or "GetByteArrayAsync" => "GET",
                "PostAsync" or "PostAsJsonAsync" => "POST", "PutAsync" or "PutAsJsonAsync" => "PUT",
                "DeleteAsync" => "DELETE", "PatchAsync" => "PATCH", "SendAsync" => "UNKNOWN", _ => null,
            };
            var url = arg?.ConstantValue.Value as string;
            if (arg is IInterpolatedStringOperation interpolated)
                url = string.Concat(interpolated.Parts.Select(part => part is IInterpolatedStringTextOperation text
                    ? text.Text.ConstantValue.Value as string ?? "" : "{param}"));
            if (verb != null && url != null)
            {
                var info = unit.Properties();
                info["http_method"] = verb;
                info["url_pattern"] = url;
                Graph.Hint(unit, source, "HTTP_CALLS", $"route:*:{verb}:{url}", syntax.Span, 0.65, info);
            }
        }
        if (method.Name is "Publish" or "Send"
            && new[] { "IBus", "IPublishEndpoint", "ISendEndpointProvider", "ConsumeContext" }
                .Any(name => owner.Contains(name, StringComparison.Ordinal)))
        {
            var message = method.TypeArguments.FirstOrDefault() ?? arg?.Type;
            if (message != null) graph.Link(unit, source, message, "PUBLISHES", syntax.Span);
        }
        if (method.Name is "AddConsumer" or "Consumer" or "RegisterConsumer" && method.TypeArguments.FirstOrDefault() is { } consumer)
        {
            var info = unit.Properties();
            var endpoint = syntax.Ancestors().OfType<V.InvocationExpressionSyntax>()
                .Select(node => model.GetOperation(node)).OfType<IInvocationOperation>()
                .FirstOrDefault(call => call.TargetMethod.Name == "ReceiveEndpoint");
            if (endpoint?.Arguments.FirstOrDefault(a => !a.IsImplicit)?.Value.ConstantValue.Value is string queue)
                info["queue_name"] = queue;
            graph.Link(unit, source, consumer, "REGISTERS", syntax.Span, info);
        }
        if (method.Name == "AddConsumers"
            && arg is IPropertyReferenceOperation { Property.Name: "Assembly", Instance: ITypeOfOperation marker })
        {
            var info = unit.Properties();
            info["registration_kind"] = "assembly";
            graph.Link(unit, source, marker.TypeOperand, "REGISTERS", syntax.Span, info);
        }
        if (method.TypeArguments.Length >= 2 && method.Name is
            "AddScoped" or "AddKeyedScoped" or "AddTransient" or "AddKeyedTransient" or "AddSingleton" or "AddKeyedSingleton")
        {
            var lifetime = method.Name.Replace("Add", "", StringComparison.Ordinal)
                .Replace("Keyed", "", StringComparison.Ordinal);
            Synthetic(unit, "Service", method.TypeArguments[0].Name, source, syntax,
                new()
                {
                    ["lifetime"] = lifetime,
                    ["interface"] = method.TypeArguments[0].ToDisplayString(),
                    ["implementation"] = method.TypeArguments[1].ToDisplayString(),
                });
        }
    }

    private static int Synthetic(Graph.Unit unit, string kind, string name, int parent,
        SyntaxNode syntax, Dictionary<string, object> metadata)
    {
        var key = $"dotnet:{unit.Project!.Path}:{kind}:{unit.File.Unit}@{syntax.SpanStart}:{name}";
        var props = unit.Properties();
        foreach (var (property, value) in metadata) props[property] = value;
        return unit.Add(new(kind, name, key, unit.Range(syntax.Span), parent, props), key);
    }

    private static bool Branch(SyntaxNode node) => VB.VisualBasicExtensions.Kind(node) is
        VB.SyntaxKind.IfStatement or VB.SyntaxKind.SingleLineIfStatement or VB.SyntaxKind.ElseIfStatement
        or VB.SyntaxKind.CaseStatement or VB.SyntaxKind.ForStatement or VB.SyntaxKind.ForEachStatement
        or VB.SyntaxKind.WhileStatement or VB.SyntaxKind.SimpleDoStatement or VB.SyntaxKind.DoWhileStatement
        or VB.SyntaxKind.DoUntilStatement or VB.SyntaxKind.CatchStatement
        or VB.SyntaxKind.AndAlsoExpression or VB.SyntaxKind.OrElseExpression
        or VB.SyntaxKind.TernaryConditionalExpression or VB.SyntaxKind.BinaryConditionalExpression;

    private static string Name(AttributeData attribute)
        => (attribute.AttributeClass?.Name ?? "").Replace("Attribute", "", StringComparison.Ordinal);
    private static string? Argument(AttributeData attribute, int index)
        => attribute.ConstructorArguments.Length > index ? attribute.ConstructorArguments[index].Value?.ToString() : null;
    private static string? Named(AttributeData attribute, params string[] names)
        => attribute.NamedArguments.FirstOrDefault(a => names.Contains(a.Key)).Value.Value?.ToString();

    private static IEnumerable<INamedTypeSymbol> BaseTypes(INamedTypeSymbol type)
    {
        for (var parent = type.BaseType; parent != null; parent = parent.BaseType)
            yield return parent;
    }

    private static bool IsDomain(ITypeSymbol type) => type.TypeKind != TypeKind.Error
        && type.SpecialType == SpecialType.None
        && !type.ContainingNamespace.ToDisplayString().StartsWith("System", StringComparison.Ordinal)
        && !type.ContainingNamespace.ToDisplayString().StartsWith("Microsoft", StringComparison.Ordinal);
}
