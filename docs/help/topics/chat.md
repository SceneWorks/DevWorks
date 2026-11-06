# Chat and conversation history

Chat holds the workspace coordinator conversation. The center header keeps New Chat and Coordinator instructions accessible. The right pane has Current first and Chat History second. Switching tabs or navigating elsewhere keeps the selected conversation and its draft. Opening the coordinator sidebar shows that same conversation beside the page you are inspecting; returning to Chat closes the sidebar.

## Messages and attachments

Write a message, attach a saved artifact if needed, and send. A message is durable only after the app confirms its save. A pending or uncertain send keeps its draft and command identity. Use the offered confirmation/recovery action before submitting another copy. Historical conversations remain available when you start a new chat. A new chat does not create projects, start workers, or resume paused work.

Choose a tier, a named model, or an available provider route independently of reasoning effort. Provider Default leaves effort to the provider; an explicit effort requires actual support. A stored conversation or route does not prove a generation runtime is available. If the app reports generation unavailable, the saved message is still history, not a claim that an assistant ran.

Streaming text is a preview until committed. Interrupted attempts are marked; a replacement attempt has its own identity. Native tool calls and results keep their order and call identities rather than being flattened into prose. Restricted provider continuation material is not a promised visible reasoning transcript.

Current includes work needing attention and verified completion notifications. Opening a completion does not dismiss it. Explicit acknowledgment dismisses its card while retaining notification history. See [Approvals](help:approvals), [Routing](help:routing-context), and [Artifacts](help:artifacts).

The current build can save and inspect conversations without a live provider. Autonomous coordination, screen-context injection, or child delegation must not be assumed from the presence of a Chat or Agents screen; an unavailable state identifies the missing runtime.
