# Retained CodeGraph C# source

These files mirror CodeGraph commit `afe73560215bbe7593fc59451de5cc3237e3eeca`.

- `CodeGraphSyntaxWalker.cs`, `RoslynExtractor.cs`, and `CSharpExtractionVersion.cs` come from `src/CodeGraph.Extractors.CSharp/`.
- `GraphNode.cs`, `EdgeType.cs`, and `NodeLabel.cs` come from `src/CodeGraph.Models/`.
- `../../tests/RoslynExtractorTests.cs` comes from `src/CodeGraph.Tests/Extractors/`.

`HostTypes.cs` supplies the fields the walker uses from the frozen extractor and pipeline types. The managed worker feeds captured Roslyn syntax trees and semantic models to the retained walker. DevWorks owns project selection, source capture, graph projection and publication.
