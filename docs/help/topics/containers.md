# Containers: build, run and stop

The container tools let an agent, or you, build images and run containers through the container engine you chose. DevWorks drives the engine's own command-line tool. It does not include an engine, and it does not replace one with another.

## Which engine is used

The **Preferred runtime** setting (`containers.preferredRuntime`) names Docker, Podman or Apple. See [Container runtime preferences](help:settings-Containers). Each container request names an engine ID, and that ID must be the engine selected for the project. A different engine is refused as a conflict, and an unknown engine name is refused as unsupported.

The engine's program must be registered with the host. DevWorks does not search your PATH. If the program is missing or has changed since registration, the engine is reported unavailable.

Apple's container tool is probed only on an Apple-silicon Mac running macOS 26 or later. DevWorks can report whether it is healthy, but it refuses to build, pull or create with Apple. Those requests fail with `apple_container_lifecycle_unqualified`. Use Docker or Podman for these.

## What each operation does

- `containers.capabilities` reports, for the selected engine or one you name, whether it is Healthy, Unavailable or Unsupported, its executable and version, and a diagnostic that says why. It runs a version check and a health check, each limited to 10 seconds. A stored setting is not proof that the engine works. Read this first when something fails.
- `containers.build` builds an image from a definition file you attach as an artifact. The build context is a registered path inside the project. The request gives the image tag.
- `containers.pull` pulls an image by name. It can use a registered registry credential. The secret is read only after the request is recorded and does not appear in the command line.
- `containers.create` records a container from an image, with a command, mounts, port mappings, environment entries and named resources. The container is not started.
- `containers.start` and `containers.stop` change a container's state. Both need the container's current revision.
- `containers.exec` runs a command inside a running container, with environment entries and a timeout.
- `containers.inspect` shows the container's state, and `containers.logs` reads its log in pages from a byte offset.

A container's state moves through Creating, Created, Starting, Running, Stopping and Stopped. It can also become Failed, or Uncertain when DevWorks cannot verify what the engine did. Uncertain work stays reserved until it is checked.

## Mounts, ports and resources

A mount source is a registered path in the project. When an agent creates the container, the source must be a direct child folder of a primary repository checkout, and its name cannot start with a dot, so `.git` and similar folders can never be mounted, read-only or not. A path that resolves through a symbolic link to such a folder is also refused. A repository root that sits inside another registered root is refused.

Host ports are reserved as resources before the container is created. Name a port only in the port list. A resource name that starts with `hostport-` is refused, and so is the same host port and protocol listed twice. If another container holds the port or resource, the request waits (CapacityWait) instead of failing. See [Resources](help:resources).

## Ownership and shutdown

DevWorks labels every container it creates with the creating operation, the project and the kind of owner. DevWorks stops a container at shutdown only if its labels match what DevWorks recorded. A reused name without the labels is left alone. The shutdown pass is bounded, and anything it could not verify as stopped is marked Uncertain.

You and the host can see all containers in the project. An agent sees only its own.

## Permissions

Under the Project access profile, build, pull, create, start and exec each need an exact native-execution exception, because running a container is not confined by DevWorks. Read-only reviewers cannot build, pull, create, start, execute in or stop containers. Containers use your engine's isolation, not a DevWorks sandbox, and native executable add-ons are not container-sandboxed. See [Process tools](help:process-tools), [Permissions](help:permissions) and [Diagnostics](help:diagnostics).
