# Tool and command contributions

A Tool contribution declares an exact qualified ID, input/output schema references, local Help topic, required/optional status, cancellation contract and requested effect class. The host decides the effective effect and permissions; `ReadCandidate` is a request for classification, not permission to perform mutations.

Example contribution shape (the included bundle builder computes its schema digests):

```json
{"kind":"Tool","meta":{"id":"demo-echo/echo","inputSchema":{"schemaId":"demo-echo/data","revision":"1","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"outputSchema":{"schemaId":"demo-echo/data","revision":"1","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"helpTopic":"echo","required":true,"cancellation":"Cooperative"},"requestedEffect":"ReadCandidate"}
```

The hash above illustrates the shape only; use the builder's actual schema reference for installation. The echo helper returns the already-owned invocation input artifact as its result. It neither fabricates an artifact ID nor writes outside the broker.

For a real host action, use a `BrokerCall::Application` with the actual registered operation, exact input schema and immutable argument artifact. Other broker call kinds cover declared HTTP, inference, artifact reads/writes and state. Current invocation scope, allowed operations, tool policy, root controls, package epoch and effect classification still apply. A helper cannot call an unregistered method merely because it knows a schema name.

Keep action keys stable across uncertain replies. A repeated receipt is not a new side effect. Use a reconciler for operations that can have an uncertain effect; do not label a timeout as successful or cancelled-before-effect. A tool result, approval-required response and later operation update remain distinct. See [Protocol](help:sdk-protocol), [Permissions](help:permissions), and [Native UI](help:sdk-native-ui).
