# Revision-3 graph field table

Generated from canonical Code* authoring types.

## CodeSearchMode

Values: RankedNodes, NodeList, Unified.

## CodeSearchSelection

| Field | Type |
| --- | --- |
| `mode` | `CodeSearchMode` |
| `text` | `Content` |
| `repository_id` | `?Id` |
| `node_kind` | `?Label` |
| `internal_module` | `?Content` |

## CodeNodeSelection

`Node`: `node_id: Id`

`Name`: `name: Content`, `repository_id: ?Id`

## CodeTraceDirection

Values: Inbound, Outbound, Both.

## CodeCallSelection

| Field | Type |
| --- | --- |
| `target` | `CodeNodeSelection` |
| `direction` | `CodeTraceDirection` |
| `depth` | `Depth` |

## CodeImpactSelection

| Field | Type |
| --- | --- |
| `target` | `CodeNodeSelection` |
| `depth` | `Depth` |

## CodeSourceMode

Values: NodeWindow, WholeFile.

## CodeSourceSelection

`Node`: `node_id: Id`, `mode: CodeSourceMode`

`File`: `repository_id: Id`, `path: RelativePath`, `start_line: Counter`, `end_line: Counter`

## CodeFileSelection

| Field | Type |
| --- | --- |
| `repository_id` | `Id` |
| `path` | `RelativePath` |
| `line` | `?PositiveCounter` |
| `depth` | `?Depth` |

## CodeSourceLocation

| Field | Type |
| --- | --- |
| `snapshot` | `RepositorySnapshot` |
| `unit_key` | `RelativePath` |
| `source_digest` | `Digest` |
| `path` | `Content` |
| `start_line` | `Counter` |
| `end_line` | `Counter` |
| `start_byte` | `?Counter` |
| `end_byte` | `?Counter` |
| `language` | `?Label` |
| `legacy_citation` | `?Citation` |

## CodeProperty

| Field | Type |
| --- | --- |
| `name` | `Content` |
| `json_text` | `Content` |
| `offset` | `Counter` |
| `total_bytes` | `Counter` |
| `next_offset` | `?Counter` |

## CodeNode

| Field | Type |
| --- | --- |
| `node_id` | `Id` |
| `repository_id` | `Id` |
| `view_id` | `Id` |
| `unit_key` | `GraphUnitKey` |
| `node_kind` | `Label` |
| `name` | `Content` |
| `qualified_name` | `Content` |
| `internal_module` | `?Content` |
| `parent_id` | `?Id` |
| `source` | `?CodeSourceLocation` |
| `properties` | `Rows<CodeProperty>` |
| `properties_complete` | `bool` |
| `do_not_trust` | `bool` |
| `trust_score` | `?Probability` |
| `description` | `?Content` |
| `analysis_confidence` | `?Label` |
| `confidence` | `?Label` |
| `extraction_source` | `?Label` |
| `fallback_reason` | `?Content` |
| `trust_revision` | `Revision` |
| `content_digest` | `?Digest` |
| `view_digest` | `Digest` |
| `content_identity` | `Content` |

## CodeEdge

| Field | Type |
| --- | --- |
| `edge_id` | `Id` |
| `source_id` | `Id` |
| `target_id` | `Id` |
| `source_repository_id` | `Id` |
| `target_repository_id` | `Id` |
| `producing_unit` | `?GraphUnitKey` |
| `edge_kind` | `Label` |
| `resolution` | `?Label` |
| `confidence` | `?Probability` |
| `confidence_band` | `?Label` |
| `properties` | `Rows<CodeProperty>` |
| `properties_complete` | `bool` |
| `source_unit_key` | `GraphUnitKey` |
| `target_unit_key` | `GraphUnitKey` |

## CodeSearchRepository

| Field | Type |
| --- | --- |
| `repository_id` | `Id` |
| `name` | `Content` |
| `description` | `?Content` |
| `language` | `?Label` |
| `framework` | `?Label` |
| `foundational` | `bool` |

## CodeSearchHit

`Node`: `node: CodeNode`

`Repository`: `repository: CodeSearchRepository`

## CodeSearchResult

| Field | Type |
| --- | --- |
| `mode` | `CodeSearchMode` |
| `items` | `Rows<CodeSearchHit>` |
| `total` | `?Counter` |
| `low_confidence_count` | `Counter` |

## CodeNodesResult

| Field | Type |
| --- | --- |
| `nodes` | `Rows<CodeNode>` |
| `total` | `?Counter` |
| `low_confidence_count` | `Counter` |
| `selected_node_id` | `?Id` |

## CodeGraphResult

| Field | Type |
| --- | --- |
| `nodes` | `Rows<CodeNode>` |
| `edges` | `Rows<CodeEdge>` |
| `low_confidence_count` | `Counter` |
| `unmapped_inbound_edges` | `?Counter` |

## CodeNodeDetail

| Field | Type |
| --- | --- |
| `node` | `CodeNode` |
| `neighbors` | `Rows<CodeNode>` |
| `incoming` | `Rows<CodeEdge>` |
| `outgoing` | `Rows<CodeEdge>` |
| `cross_repository` | `Rows<CodeEdge>` |
| `relationships_complete` | `bool` |

## CodeSourceText

| Field | Type |
| --- | --- |
| `source` | `CodeSourceLocation` |
| `node_id` | `?Id` |
| `text` | `GraphText` |
| `start_byte` | `Counter` |
| `end_byte` | `Counter` |
| `total_bytes` | `Counter` |
| `first_line` | `Counter` |
| `last_line` | `Counter` |
| `node_start_line` | `?Counter` |
| `node_end_line` | `?Counter` |

## CodeCount

| Field | Type |
| --- | --- |
| `name` | `Content` |
| `count` | `Counter` |

## CodeRepositoryLink

| Field | Type |
| --- | --- |
| `source_repository_id` | `Id` |
| `target_repository_id` | `Id` |
| `edge_kind` | `Label` |
| `count` | `Counter` |
| `confidence_band` | `?Label` |

## CodeOverviewRepository

| Field | Type |
| --- | --- |
| `repository_id` | `Id` |
| `name` | `Content` |
| `source_group` | `?Content` |
| `language` | `?Label` |
| `framework` | `?Label` |
| `foundational` | `bool` |

## CodeOverview

| Field | Type |
| --- | --- |
| `repositories` | `Rows<CodeOverviewRepository>` |
| `dependencies` | `Rows<CodeRepositoryLink>` |

## CodeHotspot

| Field | Type |
| --- | --- |
| `name` | `Content` |
| `qualified_name` | `Content` |
| `fan_in` | `Counter` |
| `reason` | `Content` |

## CodeSupertype

| Field | Type |
| --- | --- |
| `name` | `Content` |
| `qualified_name` | `Content` |
| `node_kind` | `Label` |
| `subtype_count` | `Counter` |
| `external` | `bool` |

## CodePackageDependency

| Field | Type |
| --- | --- |
| `name` | `Content` |
| `ecosystem` | `Label` |
| `version` | `?Content` |
| `dependent_count` | `Counter` |

## CodeArchitecture

| Field | Type |
| --- | --- |
| `repository_id` | `Id` |
| `summary` | `?Content` |
| `confidence` | `?Label` |
| `node_counts` | `Rows<CodeCount>` |
| `hotspots` | `Rows<CodeHotspot>` |
| `inbound` | `Rows<CodeRepositoryLink>` |
| `outbound` | `Rows<CodeRepositoryLink>` |
| `packages` | `Rows<CodePackageDependency>` |
| `package_count` | `Counter` |
| `package_counts_by_ecosystem` | `Rows<CodeCount>` |
| `supertypes` | `Rows<CodeSupertype>` |

## CodeRisk

Values: Critical, High, Medium, Low, None.

## CodeAffectedNode

| Field | Type |
| --- | --- |
| `node` | `CodeNode` |
| `depth` | `Counter` |
| `edge_kind` | `Label` |
| `risk` | `CodeRisk` |
| `risk_factors` | `Rows<Content>` |
| `low_confidence` | `bool` |

## CodeImpactSummary

| Field | Type |
| --- | --- |
| `total_affected` | `Counter` |
| `cross_repository_count` | `Counter` |
| `critical_count` | `Counter` |
| `high_count` | `Counter` |
| `medium_count` | `Counter` |
| `low_count` | `Counter` |
| `affected_repository_ids` | `Rows<Id>` |
| `low_confidence_count` | `Counter` |

## CodeImpact

| Field | Type |
| --- | --- |
| `found` | `bool` |
| `path` | `?Content` |
| `language` | `?Label` |
| `language_indexed` | `?bool` |
| `blast_radius` | `CodeRisk` |
| `changed` | `Rows<CodeAffectedNode>` |
| `affected` | `Rows<CodeAffectedNode>` |
| `cross_repository` | `Rows<CodeRepositoryLink>` |
| `summary` | `CodeImpactSummary` |
| `unmapped_inbound_edges` | `?Counter` |

## CodeTraversalEntry

| Field | Type |
| --- | --- |
| `node` | `CodeNode` |
| `depth` | `Counter` |
| `parent_id` | `?Id` |
| `edge_kind` | `?Label` |
| `edge_id` | `?Id` |

## CodeCallTrace

| Field | Type |
| --- | --- |
| `start` | `?CodeNode` |
| `entries` | `Rows<CodeTraversalEntry>` |
| `low_confidence_count` | `Counter` |
| `unmapped_inbound_edges` | `?Counter` |

## CodeLineage

| Field | Type |
| --- | --- |
| `model_name` | `Content` |
| `model` | `?CodeNode` |
| `producers` | `Rows<CodeTraversalEntry>` |
| `consumers` | `Rows<CodeTraversalEntry>` |
| `cross_repository` | `Rows<CodeEdge>` |
| `related_nodes` | `Rows<CodeNode>` |
| `low_confidence_count` | `Counter` |
| `unmapped_inbound_edges` | `?Counter` |

## CodeRelatedNode

| Field | Type |
| --- | --- |
| `node` | `CodeNode` |
| `edge_kind` | `Label` |
| `low_confidence` | `bool` |

## CodeRelated

| Field | Type |
| --- | --- |
| `nodes` | `Rows<CodeRelatedNode>` |
| `low_confidence_count` | `Counter` |
| `unmapped_inbound_edges` | `?Counter` |

## CodeSemanticGroup

| Field | Type |
| --- | --- |
| `index` | `Counter` |
| `crates` | `Rows<Content>` |
| `outcome` | `Label` |
| `failure_code` | `?Label` |
| `detail_code` | `?Label` |

## CodeRustSemantic

| Field | Type |
| --- | --- |
| `status` | `Label` |
| `manifest` | `?Content` |
| `group_count` | `Counter` |
| `semantic_groups` | `Counter` |
| `fallback_groups` | `Counter` |
| `skipped_groups` | `Counter` |
| `tree_sitter_groups` | `Rows<CodeSemanticGroup>` |

## CodeDependencies

| Field | Type |
| --- | --- |
| `repository_id` | `Id` |
| `inbound` | `Rows<CodeRepositoryLink>` |
| `outbound` | `Rows<CodeRepositoryLink>` |
| `rust_semantic` | `CodeRustSemantic` |

## CodeSchemaProperty

| Field | Type |
| --- | --- |
| `owner_kind` | `Label` |
| `name` | `Content` |
| `description` | `Content` |

## CodeSchema

| Field | Type |
| --- | --- |
| `node_kinds` | `Rows<Label>` |
| `edge_kinds` | `Rows<Label>` |
| `properties` | `Rows<CodeSchemaProperty>` |
| `notes` | `Rows<Content>` |
| `artifact` | `?ArtifactRef` |

## CodeRepositoryStatus

| Field | Type |
| --- | --- |
| `repository_id` | `Id` |
| `freshness` | `IndexFreshness` |
| `languages` | `Rows<LanguageCoverage>` |
| `semantic_packs` | `Rows<SemanticPackStatus>` |
| `overlays` | `Rows<OverlayStatus>` |

## CodeScopeStatus

| Field | Type |
| --- | --- |
| `stamp` | `ReadStamp` |
| `coverage` | `Coverage` |
| `current` | `bool` |
| `unavailable_reason` | `?Label` |
| `repositories` | `Rows<CodeRepositoryStatus>` |

