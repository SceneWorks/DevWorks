# Providers and routing settings

Tier membership is workspace-scoped and allocates eligible configured members round-robin. Defaults are Max: fable-latest/astra-latest; High: opus-latest/sol-latest; Mid: sonnet-latest/terra-latest; Low: haiku-latest/luna-latest. These names do not establish availability; connection inventory, enabled state, constraints, current capability, effort and cooldown checks still decide eligibility. Model labels must not encode effort.

## DevWorks Auto

Choose **DevWorks Auto** in the conversation route selector. To apply it by default to new managed assignments, enable **DevWorks Auto for new managed sessions** in Providers & routing. Expand **DevWorks Auto rubric** to see how the classifier chooses Low, Mid, High, or Max.

The first assignment supplies the prompt and its role or task context. DevWorks tries eligible SystemOne models first, then eligible Low models. It saves the selected tier for the session. If a generation model becomes unavailable, DevWorks can choose another eligible model in that tier. OpenRouter Auto and Auto Beta are separate route choices.

## Apply a change

Choose the intended scope, read the effective value/source, edit, validate, and save or propose through the app. Keep the original draft and operation identity if the result is uncertain. For a revision conflict, refresh and explicitly review/rebase before retrying. Adoption timing and harness-enforced restrictions are listed in the [Complete settings reference](help:settings-reference).

See [Settings overview](help:settings), [Permissions](help:permissions), and [Recovery](help:recovery).
