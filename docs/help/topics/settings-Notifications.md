# Notifications and power settings

Notification preferences and OS permission are distinct. Desktop-required-input alerts use the actual persistent recipient and current settings; denied/unavailable delivery keeps the in-app notification. Reading and acknowledgment never approve decisions or resume work. Keep Awake defaults off; when enabled it asks the OS to prevent idle sleep while the app is open, even without active work. It does not override explicit sleep, lid close, shutdown, or every power policy. Closing the app or turning it off releases that request.

## Notification defaults

Notification preferences are per human recipient. Desktop notifications default enabled; an empty kinds list means all categories. This preference does not grant OS notification permission. A focused app uses its persistent in-app surface; required-input alerts while unfocused still depend on native permission, app identity and current source eligibility. Reading, marking read, and explicit acknowledgment remain distinct from deciding an approval.

## Apply a change

Choose the intended scope, read the effective value/source, edit, validate, and save or propose through the app. Keep the original draft and operation identity if the result is uncertain. For a revision conflict, refresh and explicitly review/rebase before retrying. Adoption timing and harness-enforced restrictions are listed in the [Complete settings reference](help:settings-reference).

See [Settings overview](help:settings), [Permissions](help:permissions), and [Recovery](help:recovery).
