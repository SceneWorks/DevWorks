# Settings scopes, proposals, and adoption

Settings shows current effective values and where they came from. Choose Workspace or the selected Project before editing. Inherited settings resolve Workspace then Project; workspace-only settings cannot be overridden per project. A project reset removes its override rather than overwriting the workspace value.

## Save deliberately

Review the value, source revision, scope, and when it takes effect. Some settings apply immediately, some at the next safe step or admission, and repository base-directory changes affect new clones only. A stored preference does not install the component it describes. Unavailable adapters stay unavailable.

Human saves and agent proposals use the shared native validation and revision checks. Agent-drafted changes that need approval remain proposals until an exact human decision. A stale save cannot overwrite a concurrent edit. An uncertain save keeps the draft and command identity; confirm that command before retrying. Help content cannot supply authority to change a setting.

Harness-enforced values are visible but not editable relaxations. Read-only reviewer roles, protected Git operations, exact approvals, current root controls, and scope restrictions still apply even under Full access. See [Permissions](help:permissions).

The [Complete settings reference](help:settings-reference) lists the maintained keys, defaults, scope, adoption timing, and descriptions. Provider connections have their own [connection guide](help:settings-connections). Role instructions, tools, storage, and other section topics are linked from their Settings sections.

Do not place credentials in ordinary setting text. Use the app's native credential intake so the secret value remains in its secret facility and only an opaque reference enters settings/history.
