# Convention interface checkpoint

Source baseline: `2a11778746c8cce09b4c7fe3821502bdb2ad2685`. Retained reference:
CodeGraph `afe73560215bbe7593fc59451de5cc3237e3eeca`.
This records the assembled native interface, not completed integrated acceptance. CONV-01 remains
Confirmed and CONV-02 remains Proposed. The root delivery owns tracker and publication.

The existing 13 operation names are retained. Revision 3 uses dedicated convention DTOs;
`InstructionRecord`, role and skill records are unchanged. Basic legacy results project only
actual content references and their independently recorded approval fact. Active/Retired state
never means approved; an unrepresentable legacy title or view is refused without truncation.

| Operation | Current semantics/interface |
|---|---|
| list | Scoped, applicability/lifecycle-filtered catalog and exact scope revision |
| read | ID, scoped root-exact/unique-fuzzy slug, or separate exact literal hierarchical path; historical revision and bounded Markdown/Raw/Manifest/SectionPath or binary resource pages |
| edit | Existing content reference and owner CAS, plus optional title/slug/parent/order/raw/resource changes |
| version | Immutable owner snapshots with current and historical visibility |
| applicability | Applicability labels and explicit Active/Retired transition; Deleted is not an activation choice |
| retire | Owner retirement or scoped cancellation of an actual owned import/index operation; authored bytes/references remain retained |
| index | One exact owner body revision, or all document bodies of an explicitly named scope at its scope revision; attachment bodies are never extracted or embedded |
| import | Explicit portable versioned manifest artifact and exact registered schema; creates a convention without foreign author authority |
| delete | Owner/subtree deletion, preserving truthful metadata/history and independent caller artifacts |
| move | Source CAS plus independently authorized destination, parent/order and historical resource association |
| resources | Virtual document catalog, exact root-slug document view, or separate exact-owner/version attachment metadata listing |
| delete_resource | Exact registered resource ID and owning convention CAS |
| search | Actual matched source revision/chunk, excerpt, score and explicit lexical/hybrid availability/coverage |

ID operations may name an explicit current scope in revision 3. Older requests derive the
existing invocation's project/general view. Workspace scope means general guidance, not an
implicit enumeration of other projects. SQL scope filters precede materialization/ranking.
Moving a record must not grant access to its old project content or resources.

`ReadInput.path` is optional in revision 3 and exclusive with ID, slug and operation
selectors. It requires an explicit scope and 1–4 literal components of at most
200 Unicode scalar values each. Empty, whitespace, NUL and slash-containing
components remain literal. This preserves the HTTP hierarchy surface separately
from the MCP tool's literal root-exact then unique-fuzzy slug/title lookup. Virtual
resource document reads use only the exact root slug. No selector silently changes
from an exact hierarchy into a fuzzy match.

The existing shared Knowledge policy remains in force: an explicit Project read
inherits Workspace and that Project's readable guidance. It does not enumerate
other projects. Duplicate matching roots in those inherited scopes are ambiguous;
parent IDs then bind each exact hierarchical step. This is the existing policy,
not a new visibility bypass.

`ConventionSectionPath` contains exactly one bounded inline text or source-backed
descriptor: the actual immutable indexed body reference, captured owner revision,
and at most three original UTF-8 heading spans. Small Content-valid headings may
be cached once per section; source spans are always retained. Full breadcrumbs are
never copied into per-result artifacts or per-chunk strings. `read` with format
`SectionPath` requires the exact generation/chunk selector and owning convention.
The same Conventions authority streams exact rendered passages without a broader
artifact-tool permission or fabricated Host invocation. History, lifecycle, scope,
source, generation, cursor bounds and controlled cancellation remain owning checks.

Passages are bounded verbatim `GraphText`: source-legal NUL and UTF-8 are preserved. Both byte
and requested Unicode-character ceilings apply, with exact continuation. Source documents are
not limited to one response passage. Metadata retains the actual nonblank 500-character title and verbatim 0–200-character slug
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

Search indexes only current Active convention body text. Retired/deleted entries
remain available through authorized catalog/history operations, without an invented
historical RAG filter. Actual attachment links remain ordinary indexed body text;
attachment bytes are never dereferenced for indexing. Optional retrieval custody
fields expose only the genuine protected embedding request/preparation operation.
A confirmation retains the exact original query ID/frame; changed source or
configuration refuses before new admission. No reason string is an authority token.

Creation order in the portable format is optional; see [the source-bound order
qualification](portable-order.md). Omission resolves inside atomic publication from
the actual scoped sibling maximum (empty maximum -1), preserving first 0, explicit
zero and signed 32-bit unchecked wrapping.
