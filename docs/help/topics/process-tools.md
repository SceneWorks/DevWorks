# Process tools: running commands

The process tools let an agent, or you, run a command on this computer through DevWorks. DevWorks owns each process it starts. It records who started it, bounds its time and output, and can stop the whole process tree.

## Run a command

`process.execute` takes a process specification, not a command line:

- **Executable.** An ID of a program registered with the host. You cannot give a path. DevWorks resolves the ID to the registered file and checks that file again just before launch. An unknown ID is refused as NotFound, and a file that changed since registration is refused as SourceChanged.
- **Arguments.** A list of text arguments. They are never joined into a shell string.
- **Working directory.** A registered root and a relative path inside it. A project can use only roots registered to that project. A path that escapes the root through a symbolic link is refused.
- **Environment.** Name and value entries, or a reference to a registered credential. A credential is read only after the command is recorded and never appears in the receipt, history or command line. Duplicate names are refused.
- **Time and output limits.** A timeout from 1 ms to 3,600,000 ms (one hour) and an output limit from 1 byte to 1,048,576 bytes. A value of zero or above these limits is refused.
- **Profile.** Only `Full` runs. `Full` uses your ordinary user rights and is not a sandbox. `Project` and `Confined` are refused as unsupported, because no confined launcher exists. DevWorks does not quietly upgrade them to `Full`. Under the Project access profile, a command needs an exact native-execution exception that you grant.

DevWorks checks every command against its shell safety rules first. A blocked command never starts. A command that needs your decision starts only after you approve that exact request. The request returns an operation receipt at once. The process record, which carries the ID you need for output and cancel, is available when the launch settles.

## Read output and cancel

`process.output` returns a page of combined stdout and stderr, from the byte offset you give (`afterBytes`) up to the number of bytes you ask for (`maxBytes`). The output is redacted, so secrets that appear in it are removed, including a secret that is split across two pages. Only the first part of the output up to the limit is kept. The record notes that later bytes were discarded, and the process still runs to its end.

`process.cancel` stops a process. Give the process ID and the revision you last saw. A process that has not launched yet is cancelled before dispatch. A running process gets a cooperative stop, then a forced kill of the whole tree after a short grace period. DevWorks reports the process as Reaped only after it has verified the tree is gone. If it cannot verify that, the process is marked Uncertain and the resource it holds stays reserved. Only a person can declare a Lost or Uncertain process stopped.

You and the host can see every process in the project. An agent sees only its own. Anyone else gets NotFound.

After a restart DevWorks reconciles processes from the earlier run. It kills a recorded process that is still alive, marks a vanished one Reaped, and marks a launch that never recorded its identity as Lost.

## Install project dependencies

`process.install_project_dependencies` installs a repository's dependencies from its manifest. Name the repository and its current revision, attach the manifest as an artifact, and use the `Full` profile. The manifest must match a file at the top of the checkout exactly (same SHA-256 and length, at most 8 MiB). An agent uses the worktree it owns, and a project member without one gets PermissionDenied. DevWorks runs the install command for the manifest it found:

- `Cargo.toml` or `Cargo.lock`: `cargo build` (with `--locked` for the lock file)
- `package-lock.json`: `npm ci`. `package.json`: `npm install`
- `pnpm-lock.yaml`: `pnpm install --frozen-lockfile`. `yarn.lock`: `yarn install --frozen-lockfile`
- `pyproject.toml` or `uv.lock`: `uv sync`. `requirements.txt`: `python -m pip install -r`
- `go.mod`: `go mod download`. `Gemfile.lock`: `bundle install`. `composer.lock`: `composer install`

The program must be registered like any other executable. An unsupported manifest is refused.

## Request a system install

`process.request_system_install` does not install anything. It stages a proposal that names a package specification artifact, its digest and a reason, and it appears in [Approvals](help:approvals). Your approval records consent only. You install the package yourself. A digest that does not match the artifact is refused.

## Settings and limits

The **Shell commands** setting (`tools.shell`) turns the process tools off for agents. Read-only reviewers cannot run commands. Watchers launch their processes through this same service and share its limits. See [Watching background processes](help:watchers), [Agent tools](help:settings-Tools), [Resources](help:resources), [Permissions](help:permissions) and [Recovery](help:recovery).
