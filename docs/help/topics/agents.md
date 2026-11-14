# Agents, sessions, and execution availability

The Agents & sessions destination is the place for coordinator/worker assignment visibility. A registered operation is not a running agent: an operation moves records, and an agent runs only while the host holds an activation for its root. Check the actual run, not the screen, to see what is live.

## Agent operations

- `agents.inspect` shows one assignment: its root, role, state, blockers and latest checkpoint.
- `agents.delegate` asks for a child assignment under a root. It names the root and the root's control revision, the tasks, the role, an instruction, and optionally a checkpoint to continue from. It only records the intent. The host starts the child session, and an agent cannot start itself.
- `agents.messages` sends a message to an assignment. A message has a class: instruction, status, question or evidence. `agents.inbox` reads an assignment's messages, and `agents.acknowledge` marks one read. Reading never marks a message read.
- `agents.post`, `agents.receive`, `agents.ack`, `agents.history` and `agents.events` are the project channel. An agent posts a note (info, question, request, blocker or announcement), optionally addressed to a session or to you, and optionally blocking. `receive` pages the messages for a session from a cursor. `ack` marks messages read, and a reply can resolve a request as answered or declined. A repeated post with the same idempotency key does not post twice.
- `agents.checkpoint` records an assignment's progress at one transcript position. It cannot describe a position that does not exist yet, and it cannot replace a newer checkpoint.

## Existing control foundations

Root run controls, durable sessions, ownership generations, instruction history, and operation reconciliation are separate from an agent's visible conversation. A session can remain open after an activation stops. Inspect the actual run and its blockers rather than treating an open chat as liveness.

Each feature and standalone issue has its own configured subagent allowance, default 10. There is no global subagent-count cap. This setting does not itself launch workers. Real provider capacity, repository writes, compiler/indexing work, resources, dependencies, and permissions can still restrict dispatch.

## Continuity and safe recovery

Continuing existing work preserves logical history and reconciles current ownership. A new work item starts a fresh session with a scoped brief; another conversation is not permission to duplicate an existing execution owner. Membership handoff preserves an existing worker/session only through the native quiescent, generation-fenced boundary. Late results from an older generation cannot become current completion evidence.

A message, acknowledgment, or child report is not acceptance. Completion needs the approved outcome and verification evidence. Use [Controls](help:work-controls), [Readiness](help:features), [Resources](help:resources), and [Diagnostics](help:diagnostics) to understand a wait or interruption.
