# GitHub HTTP response metadata

DW-40 adds `HttpTransport::send_with_headers` without changing the existing status/body response API. Native exchanges retain only cache, pagination, poll/retry, rate and authorization-status metadata, with an independent 32 KiB bound. Duplicate values remain ordered; lookups are case-insensitive. Debug output excludes header values and bodies. Cookies and response authorization values are not retained. Existing `send` callers do not opt into metadata retention or validation.

The native HTTP suite passes all seven cases, including conditional 304 responses, exact ETag/Last-Modified/Link values, duplicate pagination headers, metadata and body bounds, secret-safe debugging, unchanged POST/PATCH bodies, redirect refusal, timeout and unreachable-endpoint behavior. Core and adapter all-target strict Clippy passed on the initial metadata checkpoint `cfa6aa26`; the final Last-Modified addition is covered by the native suite. These are loopback HTTP tests with fake request credentials, without Git or real OS secret access.

The existing `2022-11-28` API pin remains supported until March 10, 2028; no API-version migration is needed for this work. See [GitHub API versions](https://docs.github.com/en/rest/about-the-rest-api/api-versions). The polling client must preserve conditional representations, honor endpoint poll intervals and rate/retry headers, and pace mutations; see [GitHub REST best practices](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api).

This checkpoint supplies transport metadata. The full account, polling, issue intake, resolution, event-consumer and native UI implementation remains within DW-40; this document claims no completed polling or project acceptance.
