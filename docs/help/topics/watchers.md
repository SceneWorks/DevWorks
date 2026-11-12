# Watching background processes

A watcher lets an agent start a long-running process, such as a build, a test run or a dev server, and keep working instead of waiting for it. `watch.create` launches the process through the same process tools as `process.execute`: the same registered executables and working roots, the same shell checks, and the same output and time limits. It returns the watcher's ID straight away, before the process exits.

When the process ends, DevWorks records what the process supervisor observed and delivers it once to the session that created the watcher, as a host message. The message names the watcher, the process, the outcome, the exit code or signal, the duration and the last part of the output with credentials removed. The next turn of that session reads it; the message counts as handled only when that turn completes. If DevWorks restarts first, the message is delivered after the restart, still once.

The outcome is only what was observed: Succeeded (exit code 0), Failed (another exit code, a signal, or a launch that could not start), TimedOut, Cancelled, Interrupted (DevWorks restarted and stopped the process), Lost or Uncertain. Starting a process is never reported as success.

Use `watch.list` and `watch.inspect` to see a watcher's state, elapsed time and current output tail. `watch.cancel` with `stopProcess` stops the launched process and reports Cancelled; without it the watcher stops observing and reports nothing, and the process keeps running under its owner.

Agents and add-ons cannot write watcher messages themselves. A read-only reviewer cannot start or stop a process through a watcher, and the shell-tools setting turns watchers off with the other process tools. See [Resources](help:resources), [Agents](help:agents) and [Permissions](help:permissions).
