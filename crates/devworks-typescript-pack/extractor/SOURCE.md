# TypeScript extractor source

The compiler passes and their fixtures come from CodeGraph commit
`afe73560215bbe7593fc59451de5cc3237e3eeca`, under `tools/ts-extractor`.
The test enum fixtures come from `src/CodeGraph.Models` at the same revision.
DevWorks preserves the compiler code and the fixture suite here. The managed
package ships the compiler passes and the DevWorks adapter with their runtime
dependencies. The test server and lint endpoint are used by the upstream tests.

Use `npm ci --ignore-scripts` to install the pinned build dependencies and
`npm test` for the compiler fixtures.
