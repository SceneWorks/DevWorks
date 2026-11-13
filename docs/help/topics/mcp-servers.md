# MCP servers: connect outside tools

An MCP server gives DevWorks extra tools, resources and prompts. DevWorks is the client. These operations manage the outgoing connections. They do not make DevWorks an MCP server for other programs, and no operation for that is registered.

## Set up a server

Only a person can change a server's configuration. An agent can read the server list and use the tools you selected, but it cannot add, change, select, classify, disable or remove a server.

1. `mcp.configure` creates or edits a server. Give it a name, a scope (the workspace, or one project) and a transport. An edit needs the server's current revision.
   - **stdio** starts a local program. The transport carries a process specification, so the program must be a registered executable, and the same limits and safety checks as [Process tools](help:process-tools) apply.
   - **HTTP** connects to an address. A credential reference is optional, and it must be a handle that starts with `mcp-`. The saved record holds the handle, never the secret.
2. `mcp.connect` connects and reads what the server offers. It returns at once and finishes in the background. `mcp.discover` connects again and reads the catalog again. It needs the catalog revision you last saw, so you do not overwrite a newer catalog without knowing.
3. `mcp.select_tools` chooses which tools agents may use, from the catalog. The first time a server is discovered, DevWorks selects up to 128 of its tools that are read-only. For each tool, your own classification (see the next step) decides if you set one, and otherwise the server's read-only hint does. Review that list after connecting, and clear any tool you do not want agents to use. Later discoveries do not add tools. A selected tool whose definition changed on the server drops out of the selection until you select it again. A selection is checked against the server's current catalog and fails if the catalog moved. The host-made `createMessage` entry, which lets that server ask DevWorks to run a model request for it, is never selected automatically. Select it only if you want that.
4. `mcp.classify_tool` records your decision on whether a tool is read-only, or returns it to the server's own hint. A server's read-only label is only a hint, and it never makes a tool available to a reviewer.

`mcp.servers` lists servers page by page. `mcp.capabilities` lists a server's tools, resources and prompts for a catalog revision. `mcp.resources` lists a server's resources or reads one by address, and `mcp.prompts` lists its prompts or renders one with arguments. These reads need the catalog revision and are limited in size.

## Turn a server off

`mcp.disable` keeps the server's record and stops using it. `mcp.remove` deletes the record. Both need the server's current revision. A disabled server cannot be discovered until a person turns it back on. Connecting it again, as a person, enables it. An agent that tries to connect a disabled server is refused.

When something fails, the server shows a failure message and an unavailable state. Fix the cause, then connect again. If a later discovery no longer lists a tool you selected, that tool stays in the list as unavailable and is not offered to agents. It is selected again when the server lists it again unchanged.

## What agents see

A selected tool reaches an agent under a generated name that starts with `mcp_`. An agent can call it only if its project, its tool selection and its role allow it. Read-only reviewers cannot use MCP tools at all. Under the Project access profile, `mcp.connect` and `mcp.discover` also need an exact native-execution exception, because a stdio server runs code with your user rights.

If this build has no MCP client runtime running, connect and discover are refused as an unsupported target (`mcp_runtime`). You can still configure, select, disable and remove servers.

## Scope and projects

A workspace server is available to every project. A project server is available to that project only. An archived project's servers cannot be changed. When a project is purged, its servers go with it. See [Agent tools](help:settings-Tools), [Permissions](help:permissions), [Diagnostics](help:diagnostics) and [Add-ons](help:addons).
