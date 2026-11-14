# Watching background processes

A watcher lets an agent start a long-running process, such as a build, a test run or a dev server, and keep working instead of waiting for it. `watch.create` launches the process through the same process tools as `process.execute`: the same registered executables and working roots, the same shell checks, and the same output and time limits. It returns the watcher's ID straight away, before the process exits.

When the process ends, DevWorks records what the process supervisor observed and delivers it once to the session that created the watcher, as a host message. The message names the watcher, the process, the outcome, the exit code or signal, the duration and the last part of the output with credentials removed. The next turn of that session reads it; the message counts as handled only when that turn completes. If DevWorks restarts first, the message is delivered after the restart, still once.

The outcome is only what was observed: Succeeded (exit code 0), Failed (another exit code, a signal, or a launch that could not start), TimedOut, Cancelled, Interrupted (DevWorks restarted and stopped the process), Lost or Uncertain. Starting a process is never reported as success.

A watcher takes exactly one source:

- **Process.** The behavior described above.
- **Probe.** DevWorks runs a command, then runs it again after an interval, waiting twice as long after each failed run up to eight intervals, until a run meets the condition: an exit code (0 by default) and, optionally, an output pattern. The watcher then reports Succeeded with the number of runs. A run that cannot start, or is lost or uncertain, ends the watcher with that outcome.
- **Timer.** It fires at a time or after a delay and reports Succeeded. A timer starts nothing, so every role can create and cancel one.
- **File.** DevWorks reads the lines of one regular file inside a registered root and looks for a pattern. The file may not exist yet.

A process or file watcher can also count matches of a regular expression. Without `each`, the first match completes the watcher. With `each`, matches are reported together, at most once per minimum interval (30 seconds by default), and each report carries its count. The final outcome includes any matches not yet reported, so none is lost. A file watcher ends at its maximum lifetime or when you cancel it.

A probe or file watcher always ends. Its maximum lifetime is set on the watcher or comes from `watch.maxLifetimeMs` (24 hours by default, workspace or project). When it passes, the watcher reports TimedOut and stops a run in flight.

A watcher follows its owner. If the owning session ends, or the project is archived or deleted, the watcher is cancelled, its process is stopped and nothing is delivered. While the owner is paused or held, events wait and arrive in order after Continue. At shutdown, a process DevWorks stopped is reported Interrupted. A probe runs again after you reopen the app and continue, and a timer that came due while the app was closed fires once.

A backup check runs on the watcher timer when `watch.backupEnabled` is on (the default), at the interval in `watch.backupIntervalMs` (15 minutes by default, at least 1 minute). It settles outcomes the main pass missed, reports a process the system says is gone as Lost, and delivers any message still waiting. A message sent this way starts with "Backup delivery" and is recorded in History and Diagnostics. It also reminds you about a probe that has not run for twice its interval.

Use `watch.list` and `watch.inspect` to see a watcher's state, elapsed time and current output tail. `watch.cancel` with `stopProcess` stops the launched process and reports Cancelled; without it the watcher stops observing and reports nothing, and the process keeps running under its owner.

In the app, the Watchers screen lists every watcher in every project, newest first. The Current tab beside a conversation lists that conversation's watchers, and a feature's detail lists the watchers of the agents working on it. Each row shows the project, feature, work item, assignment and agent that own the watcher, its source, its state, how long it has run, when it last checked its source, and the last lines of its output. **Cancel watcher** stops observing; **Cancel and stop process** also stops what the watcher launched. The lists read again every few seconds while they are shown. Watcher messages appear in the conversation that owns the watcher, including messages a backup delivery found.

Agents and add-ons cannot write watcher messages themselves. A read-only reviewer cannot start or stop a process through a watcher, and the shell-tools setting turns watchers off with the other process tools. See [Resources](help:resources), [Agents](help:agents) and [Permissions](help:permissions).
