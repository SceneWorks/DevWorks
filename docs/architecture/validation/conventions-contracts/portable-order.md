# Portable creation order qualification

Retained source: CodeGraph `afe73560215bbe7593fc59451de5cc3237e3eeca`.
`src/CodeGraph.Services/WikiService.cs:122` and `:142` resolve the sibling
maximum plus one for root and child creation respectively.
`src/CodeGraph.Data.MariaDb/MySqlWikiStore.cs:61-68` returns `-1` when
there are no siblings. The resulting first order is therefore `0`.
The service uses ordinary `int` addition without a checked context or project
checked-overflow setting; native creation deliberately uses `wrapping_add(1)`
to retain the `i32::MAX` to `i32::MIN` boundary.

The unpublished version-1 portable manifest makes `sortOrder` optional.
The editor writer omits it only for `None`; `Some(0)` remains an explicit zero.
Export continues to emit the actual stored order. Import resolves omission
from the actual destination scope and parent in the same publication unit of
work that checks current authority, source identity and catalog CAS. The SQL
maximum includes current non-deleted siblings, including retired entries.
No caller-side maximum or stale preflight default is used.

Actual native coverage (no provider execution or direct catalog inserts):

- `conventions_import::service::omitted_order_appends_but_explicit_zero_remains_an_explicit_choice`
  imports through the registered `ConventionService`, gateway, managed artifact
  owner and production consumer: omitted orders are 0 then 1, explicit 0 stays
  0, and the next omitted order is 2.
- `conventions_import::service::omitted_order_retains_frozen_unchecked_i32_maximum_wrapping`
  imports explicit maximum, omitted minimum and explicit zero.
- The three streaming-writer unit tests retain literal empty-slug/parent-path
  values, distinguish missing order from explicit zero, preserve exact binary
  and text bytes, and stream a full legal NUL document without a manifest buffer.

Both native cases passed (1.92 seconds). All three writer cases passed
(3.86 seconds). Strict Clippy passed for `devworks-core --lib` and
`devworks-engine --test conventions_import` (31.62 seconds).
This changes no public operation DTO, qualified schema revision or old
revision-1/2 schema bytes; the portable schema digest is derived from its
registered source file by the existing owner.
