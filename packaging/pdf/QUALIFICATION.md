# Native PDF qualification

The sidecar is built from the locked desktop package. The host finds its one fixed executable
beside the desktop executable, measures the exact input digest, and opens an owned native
process with a private anonymous pipe. The request names an operation and optional page and
carries document bytes. The reply must match the source digest and operation before the host
uses it. The sidecar has no URL, filesystem path, credentials, password, or executable inputs.

Lopdf reads strict native PDF text in content-stream order, one original page at a time.
Hayro renders explicitly selected pages and uses embedded standard-font substitutes.
Forms, interactive widget data and unsupported positioning report partial text coverage. The
worker returns encrypted input as a typed refusal, including files with an empty opening password.
The original AD65 probe also verified explicit-password opening. The internal v1 worker protocol
accepts a page operation and PDF bytes. It records a verified blank only if both native
rendering and the page content/annotation inspection agree.

The defaults cap input at 32 MiB, page count at 512, Lopdf stream decoding and normalized
text at 16 MiB each, and a raster at 2 million pixels and 2 MiB of static PNG. Hayro resource decoding runs within
the worker allocation and time budgets. The host limits active
workers to two per desktop host and imposes a 30-second deadline. Native process ownership
enforces no child processes and enables stop and reap on cancel or timeout. Linux and Windows
use a 512 MiB OS process limit. The sidecar also caps Rust allocations at 448 MiB. On macOS
the host samples the owned process's resident memory and stops it above 512 MiB. The allocation
cap applies within macOS sampling intervals. The OS controls cover process creation, memory and lifetime. The Rust parsers receive managed
bytes through the private channel.

Run the **Native PDF worker qualification** workflow to build each architecture natively,
stage the executable and notices, unpack the resulting qualification archive, and exercise
its worker through the real process host. Each target publishes binary and archive sizes,
SHA-256 digests, and platform/test evidence. The macOS qualification app and embedded worker
are ad-hoc signed for this test. Production notarization, Windows installer signing, and
OS-version qualification belong to the actual distribution process. Component results name the
OS and architecture that actually ran the tests.
