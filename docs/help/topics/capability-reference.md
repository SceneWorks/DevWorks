# Operation reference and local documentation refresh

The generated capability reference is derived from the host's current operation registry, registered implementations, schema revisions, effects, permissions, and availability. It complements this manual's workflows; a bare schema description does not explain the whole UI.

Search an exact operation name, inspect its supported mode and schema revision, then read its current input/result/error contract. A generated descriptor for an unregistered or unqualified handler is not a working operation. Disabled, read-only, project-bound, and unavailable states must remain visible. Help retrieval itself never grants permission to execute the operation it describes.

## Refresh documentation

Use the supported `help.generate_capability_docs` action to regenerate the local reference from current host metadata. It is a journaled command, not a request to an external documentation service or language model. Confirm an uncertain refresh with its original identity. Use the returned reference/revision/digest and bounded Help reads.

This retains the practical purpose of regenerating MCP tool documentation: definitions should match the installed tool surface. DevWorks does not require the old web server, hosted pages, or background job system to read Help. A refresh must not advertise an incoming MCP server, tool, or package contribution that this host cannot execute.

Package guides retain their package version and manifest digest separately from app/generated docs. Update or remove an add-on through [Add-ons](help:addons), then refresh the guide listing. See [Using Help](help:help), [SDK protocol](help:sdk-protocol), and [Permissions](help:permissions).
