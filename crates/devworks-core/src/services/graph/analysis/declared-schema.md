## Node Types
Repository, DotnetProject, Namespace, Folder, File, Class, Interface,
Enum, Struct, Record, Function, Method, Property, Constructor, Delegate,
Route, Service, Table, View, StoredProcedure, Event, Queue, Exchange,
Component, Module, Job, NuGetPackage, Package, ExternalSymbol

## Edge Types
CONTAINS_FILE, CONTAINS_FOLDER, CONTAINS_NAMESPACE, CONTAINS_PROJECT,
DEFINES, DEFINES_METHOD, CALLS, IMPORTS, IMPLEMENTS, INHERITS,
USES_TYPE, INJECTS, HTTP_CALLS, HANDLES, QUERIES, PUBLISHES, CONSUMES,
REFERENCES_PACKAGE, RENDERS, SUBSCRIBES, FILE_CHANGES_WITH, SCHEDULES

## Key Properties
- Route: http_method, route_template, handler
- Service: lifetime, interface, implementation
- Event: queue_name, exchange_name
- Method: signature, return_type, is_async, complexity, is_entry_point
- CALLS edge: confidence, confidence_band
- HTTP_CALLS edge: url_pattern, http_method, source_repo, target_repo
- Package (Cargo): ecosystem, package_name, local_name, version, package_key,
  source_kind (registry|git|path|workspace), source, is_definition, manifest_path,
  dependency_scope, workspace_inherited, optional, target, git_reference, external_path
- REFERENCES_PACKAGE cross-repo edge (Cargo): package_key, package_name, local_name,
  version, matched_by (package_key|git_url|path), confidence_band
- Package (SwiftPM, ecosystem swiftpm): package_name, identity, version, package_key,
  source_kind (local|remote|path|registry), source, location, is_definition, manifest_path,
  dependency_kind (declared|pinned), version_requirement, git_reference, revision, branch,
  external_path
- REFERENCES_PACKAGE cross-repo edge (SwiftPM): package_key, package_name, identity, version,
  matched_by (git_url to the root Package.swift of the repository at that URL | path),
  confidence_band, dependency_kind (declared | pinned: a Package.resolved pin with no
  declared dependency on the same package, e.g. Xcode or transitive)
- Rust nodes from SCIP: source (scip), scip_symbol
- ExternalSymbol: source (scip), scip_symbol, scip_external
- Tree-sitter fallback nodes: confidence (low), extractionSource, fallbackReason
- Rust Function/Method (tree-sitter): abi (the foreign ABI of an `extern "C" fn`, e.g. C)
- File: skipped_reason (oversize: over the indexing size limit, no symbols extracted),
  size_kb, size_limit_kb
- CALLS resolved by name stay within the caller's language family, plus Swift/Rust -> C and
  Swift/C -> Rust functions exported with a foreign ABI

## Swift (tree-sitter)
- Types: qualified name `{file}#type:Outer#type:Inner`. class/actor -> Class, struct -> Struct,
  enum -> Enum, protocol -> Interface; declaration_kind (class|struct|enum|actor|protocol),
  raw_type (an enum's raw-value type, e.g. String), class_bound (a protocol refining AnyObject)
- Members of an extension live under `{file}#extension:{TypePath}`; when the extended type is
  declared in another file, their DEFINES/DEFINES_METHOD edge starts at that type
- Method/Function: qualified name ends in `name(label:Type,...)` with external labels (`_` when
  unlabeled), e.g. `thumbnail(of:CGImage)`; argument_labels (external labels, `=` marks a default value, `...` a
  variadic parameter, e.g. `width=:_:`), receiver_owner, return_type, parameters
- Property nodes (`#property:{name}`): computed (computed property) or requirement (protocol
  requirement), type, is_static, accessors (get|set|modify, comma-separated); stored
  properties are not nodes
- INHERITS/IMPLEMENTS/DEFINES/DEFINES_METHOD edges resolved across files: origin_file (file that
  emitted the edge), swift_source_ref / swift_target_ref (the type reference it was resolved
  from) and swift_source_name / swift_target_name (their simple names); inheritance_inferred
  with swift_inferred_kind (a class's first supertype whose superclass-vs-protocol kind is a
  name-based guess while it is external), swift_protocol_target (a protocol refinement),
  swift_ambiguous (bound to every type of an ambiguous name, confidence low)
- `swift-external:{TypePath}` stub nodes (Class, or Interface when conformed to): a type
  declared outside the repository; properties stub, language (Swift), type_path
- SwiftPM targets: Module nodes `{repository}:{manifest}#target:{name}` (DEFINES from their
  Package definition) with module_name, target_kind (target|executableTarget|testTarget|
  macro|plugin|binaryTarget|systemLibrary), source_path, package_name, manifest_path;
  a declared Package lists the products its targets use in swift_products
- IMPORTS edges (file -> provider) for `import M`: swift_import (module), swift_module (top-level
  module), matched_by (target: an in-repo SwiftPM target | product: the declared dependency
  providing product M | identity: a declared, else pinned, package named M | external),
  imported_symbols (symbols of scoped imports such as `import struct M.T`), confidence (low
  when several packages provide M); a scoped type import of an in-repo module also links the
  file to the type (IMPORTS with imported_symbols, origin_file)
- `swift-external-module:{M}` stub nodes (Module): a module nothing in the repository
  provides (Foundation, SwiftUI, UIKit, AppKit, Combine, ...); properties stub, external,
  language (Swift), module_name
