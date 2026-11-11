# Knowledge, lessons, and decisions

Knowledge stores scoped claims, evidence, relations, and lessons. Inspect a claim's source, author, scope, current revision, evidence, and history before relying on it. Project Knowledge can reference allowed general Knowledge; scope filtering happens before retrieval and is not supplied by a model's judgment.

## Record and retrieve

Record concrete facts, decisions, constraints, failed attempts, and corrections with evidence and useful paths. Use the existing Knowledge operations to search, inspect, revise, relate, or challenge them. Lexical retrieval works without embeddings and reports its mode. Semantic support is used only when its configured embedding space and producer are available; an unavailable metric is not zero.

## Explore the graph

Entities are connected by claims that name another entity. A subgraph starts from chosen entities, claims or search words and follows active connections up to the requested depth (at most five). An entity bundle shows one entity with its claims and neighbors; a claim bundle shows one claim with its conflicts, supersession chain and evidence; an entity graph shows one entity's neighbors, or pages through the whole visible graph. Every view stays inside the scopes you can read and stops at its limits: a limited answer says it is truncated and lists frontier entities to expand next. A query answers search words with a subgraph and a Markdown summary, and a rendered summary is saved as an artifact you can open or share. Superseded and retracted claims appear only when asked for; conflicting claims are shown, never silently resolved.

## Lessons and challenges

A lesson from the user's own words takes effect at once. An inferred lesson can take effect after the configured count of distinct source events; the default recurrence threshold is three. Rules do not require a separate approval, and autonomous decisions do not require acknowledgment. Rejecting or retiring a rule is distinct from reaching its recurrence threshold.

Challenges retain their own author, evidence, open/resolved state, and resolution. The informational Knowledge review queue shows scoped open challenges, proposed candidates, and contradictions. Resolving an approval or dismissing a notification does not resolve these records. Inspect changed evidence before reopening a settled choice.

Imports, merges, deletion, and scope moves use current revisions and retain attributable history. Do not remove backing files to undo a Knowledge mutation. See [Documents and retrieval](help:documents-retrieval), [Approvals](help:approvals), and [Settings](help:settings-Knowledge).
