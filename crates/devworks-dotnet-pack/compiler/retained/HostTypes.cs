// The fields the pinned extractor consumes from CodeGraph's ICodeExtractor and PipelineTypes.
global using CodeGraph.Services;

namespace CodeGraph.Services
{
    public interface ICodeExtractor
    {
        IReadOnlySet<string> SupportedExtensions { get; }
        string GetExtractionVersion(string extension);
        Task<CodeGraph.Models.ExtractionResult> ExtractAsync(string filePath, string content,
            ExtractorContext context, CancellationToken ct = default);
    }

    public class ExtractorContext
    {
        public required string RepositoryName { get; init; }
        public required string RootPath { get; init; }
        public string? DotnetProject { get; init; }
    }
}

namespace CodeGraph.Models
{
    public record ExtractionResult
    {
        public IReadOnlyList<GraphNode> Nodes { get; init; } = [];
        public IReadOnlyList<PendingEdge> Edges { get; init; } = [];
        public IReadOnlyList<UnresolvedCall> UnresolvedCalls { get; init; } = [];
        public IReadOnlyList<UnresolvedImport> UnresolvedImports { get; init; } = [];
    }

    public record PendingEdge(string SourceQN, string TargetQN, EdgeType Type,
        Dictionary<string, object>? Properties = null);

    public enum CallReceiverKind { Unknown, Bare, Resolved, Unresolved }
    public record UnresolvedCall(string CallerQN, string CalleeName, string? ReceiverType,
        double Confidence, CallReceiverKind ReceiverKind = CallReceiverKind.Unknown,
        string? ArgumentLabels = null);

    public record UnresolvedImport(string FileQN, string ImportedNamespace,
        string? ImportedSymbol = null, string? ImportedSymbolKind = null, bool Exported = false);
}
