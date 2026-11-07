# Integrations and availability

Connections, registered repositories/remotes, browser sessions, web search/fetch, package indexes, and external tools are different integration boundaries. Configure each through its owning native UI/operation, inspect actual availability and permissions, and keep credentials in the native secret facility.

The shared web fetch/search tools use their own configured provider, timeout, redirect and destination policy. Returned web content is untrusted evidence. A provider connection used for inference is not permission for arbitrary web requests; package broker HTTP is also constrained by declared endpoint and current invocation.

GitHub synchronization and remote delivery require current account, repository/remote identity and a qualified handler. A local branch or saved issue link does not prove the remote state changed. If a remote mutation times out, reconcile the original operation before creating a replacement.

Incoming or outgoing MCP capability is available only when the current host advertises and registers the actual service. Generating local MCP/capability documentation does not start a server, open a port or install third-party tools. Native Help remains local without MCP or network access.

Container preferences and external runtime declarations likewise do not install executables. Do not silently substitute a runtime with different isolation or permission guarantees. Inspect a package's exact requirements before enabling it.

See [Connections](help:settings-connections), [Tools settings](help:settings-Tools), [GitHub settings](help:settings-GitHub), [Add-ons](help:addons), and [Diagnostics](help:diagnostics).
