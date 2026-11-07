# DW115: unresolved native shutdown history observation

PR42 head1c3da462 Ubuntu-desktop job128216749964 passed38/39 native desktop library
cases. The OAuth/started-secret-write Engine Drop case observed committed sequence26 after
capturing25, while its existing bounded shutdown, reauthentication, unchanged credential revision,
unknown-operation handle and zero-inference checks passed before the final history assertion.

The same actual native case passes locally2.96s; all39 library cases pass5.16s; twelve additional
focused executions pass in31.40s total. This does not establish the cause of the CI delta or repair
it. A bounded failure-only diagnostic now records event identities (kind/aggregate/operation),
without payloads or credentials. The complete sequence assertion and every original shutdown
bound remain unchanged. No waiting/retry/source bypass was added. Fresh CI must identify any
recurrence; no acceptance or causal-fix claim is made from these local passes.
