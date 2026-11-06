# Convention interface checkpoint

Source baseline: `2a11778746c8cce09b4c7fe3821502bdb2ad2685`. Retained reference:
CodeGraph `afe73560215bbe7593fc59451de5cc3237e3eeca`.
This is a compiling interface foundation, not completed service acceptance. CONV-01 remains
Confirmed and CONV-02 remains Proposed. The root delivery owns tracker and publication.

The existing 13 operation names are retained. Revision 3 uses dedicated convention DTOs;
`InstructionRecord`, role and skill records are unchanged. Basic legacy results project only
actual content references and their independently recorded approval fact. Active/Retired state
never means approved; an unrepresentable legacy title or view is refused without truncation.

| Operation | Current semantics/interface |
|---|---|
| list | Scoped, applicability/lifecycle-filtered catalog and exact scope revision |
| read | ID or scoped exact/unique-fuzzy slug, exact historical revision, bounded Markdown/Raw/Manifest/SectionPath passage |
| edit | Existing content reference and owner CAS, plus optional title/slug/parent/order/raw/resource changes |
| version | Immutable owner snapshots with current and historical visibility |
| applicability | Applicability labels and explicit Active/Retired transition; Deleted is not an activation choice |
| retire | Owner or exact named resource retirement; authored bytes/references remain retained |
| index | One exact owner/resource revision, or all documents of an explicitly named scope at its scope revision |
| import | Explicit portable versioned manifest artifact and exact registered schema; creates a convention without foreign author authority |
| delete | Owner or exact resource deletion, preserving truthful metadata/history and independent caller artifacts |
| move | Source CAS plus independently authorized destination, parent/order and historical resource association |
| resources | Exact owner/version or scoped resource listing, distinct identities/lifecycle/content references |
| delete_resource | Exact registered resource ID and owning convention CAS |
| search | Actual matched source revision/chunk, excerpt, score and explicit lexical/hybrid availability/coverage |

ID operations may name an explicit current scope in revision 3. Older requests derive the
existing invocation's project/general view. Workspace scope means general guidance, not an
implicit enumeration of other projects. SQL scope filters precede materialization/ranking.
Moving a record must not grant access to its old project content or resources.

`ConventionSectionPath` contains exactly one bounded inline text or a real immutable artifact
reference. `read` with format `SectionPath` additionally requires its exact generation/chunk
selector and owning convention; callers cannot submit arbitrary artifact references. This gives
Conventions-only readers bounded access without broader artifact-tool permission or a fabricated
Host invocation. History, lifecycle, scope, source and generation fences remain owning checks.

Passages are bounded verbatim `GraphText`: source-legal NUL and UTF-8 are preserved. Both byte
and requested Unicode-character ceilings apply, with exact continuation. Source documents are
not limited to one response passage. Metadata retains the actual 500/200-character title/slug
and signed 32-bit ordering constraints; these are data labels, not filesystem paths.

The registered portable format is `docs/architecture/imports/convention-manifest.v1.schema.json`.
It carries actual Markdown/raw bytes and base64 resource bytes with length/digest, rather than
references meaningful only in the exporting store. The source format carries no author assertion.
Its parent path resolves within the explicitly authorized destination scope; importing cannot
silently widen scope, replace another convention, or migrate excluded legacy wiki data.
The importer/exporter must stream source/resource payloads with bounded memory and atomically
publish a complete source-fenced catalog/resource snapshot. No full manifest or all-resource
base64 concatenation is permitted. Resource/file/quota admission uses owning configured bounds.

Indexing and search use the same owner and revision-bound source metadata. Embeddings are actual
routed/accounted inference outside SQL snapshots/writers. Normal tests use recorded vectors or a
local recorded-response transport, with no learned compute. No compatible configured route/space
means explicit lexical fallback, not a claimed hybrid result. Scheduled empty requests capture
current scope/targets before first admission and preserve the exact frame across lost ACK/replay.
