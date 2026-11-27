# .NET semantic indexing

The .NET pack enriches captured C# and Visual Basic source using Roslyn on macOS arm64. It requires a discoverable .NET 10.0.2xx SDK and .NET 10.0 runtime. Indexing uses installed framework reference metadata and in-tree project references. Project scripts, build targets, package restore, analyzers and generators do not run. NuGet declarations remain visible and unavailable assemblies appear in coverage. Structural C# and text Visual Basic indexing stay available when the pack cannot run.
