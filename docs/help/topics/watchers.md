# Watching background processes

A watcher lets an agent start a long-running process, such as a build, a test run or a dev server, and keep working instead of waiting for it. When you supply a command, `watch.create` launches it through the same process tools as `process.execute`: the same registered executables and working roots, the same shell checks, and the same output and time limits. It returns the watcher's ID straight away, before the process exits.

When the process ends, DevWorks records what the process supervisor observed and saves a host message in the session that created the watcher. The message names the watcher, the process, the outcome, the exit code or signal, the duration and the last part of the output with credentials removed. An idle agent starts a new turn. An agent already working receives the notice after its next tool result. DevWorks acknowledges the event only when a turn that received it completes. After a failed turn or restart, the agent can receive the same event ID again without adding a second message to the transcript.

The outcome is only what was observed: Succeeded (exit code 0), Failed (another exit code, a signal, or a launch that could not start), TimedOut, Cancelled, Interrupted (DevWorks restarted and stopped the process), Lost or Uncertain. Starting a process is never reported as success.

A watcher takes exactly one source:

- **Process.** The behavior described above.
- **Attached process.** In schema revision 18, pass `attachedProcess` with the running process's `pid` and `birthToken`. DevWorks resumes watching that identity after restart and reports Exited with an exit code when available, or Exited with unknown status. You can cancel the watcher and leave the process running.
- **Delegated assignment.** In schema revision 18, pass `assignmentId` for one of your direct child assignments. When it finishes, the parent receives the outcome, the child session ID and the recorded evidence. Cancelling the watcher leaves the child running.
- **Probe.** DevWorks runs a command, then runs it again after an interval, waiting twice as long after each failed run up to eight intervals, until a run meets the condition: an exit code (0 by default) and, optionally, an output pattern. The watcher then reports Succeeded with the number of runs. A run that cannot start, or is lost or uncertain, ends the watcher with that outcome.
- **Timer.** It fires at a time or after a delay and reports Succeeded. A timer starts nothing, so every role can create and cancel one.
- **File.** DevWorks reads the lines of one regular file inside a registered root and looks for a pattern. The file may not exist yet.
- **GitHub.** In `watch.create`, give `github` the project repository's ID and either a `workflowRunId` or `pullRequestNumber`. DevWorks uses the workspace's connected GitHub account and the repository's verified binding. A run watcher reports the run's conclusion and link. A PR watcher observes checks for the current PR head and waits if it sees pending checks. GitHub watchers resume when DevWorks reopens.

An attached process's birth token identifies the PID's specific lifetime. On macOS it is the native start time in seconds with six fractional digits. On Linux it is the kernel boot ID followed by a colon and the process start time in clock ticks from `/proc/<pid>/stat`. On Windows it is the process creation time as a decimal FILETIME value.

A process or file watcher can also count matches of a regular expression. Without `each`, the first match completes the watcher. With `each`, matches are reported together, at most once per minimum interval (30 seconds by default), and each report carries its count. The final outcome includes any matches not yet reported, so none is lost. A file watcher ends at its maximum lifetime or when you cancel it.

A probe, file, attached-process, delegated-assignment or GitHub watcher has a maximum lifetime set on the watcher or inherited from `watch.maxLifetimeMs` (24 hours by default, workspace or project). When it passes, the watcher reports TimedOut. A probe also stops its running command. A delegated watcher’s timeout leaves the child assignment running.

A watcher follows its owner. If the owning session ends, or the project is archived or deleted, the watcher is cancelled, any process it launched is stopped, and nothing is delivered. While the owner is paused or held, events wait and arrive in order after Continue. At shutdown, a process DevWorks stopped is reported Interrupted. A probe runs again after you reopen the app and continue, and a timer that came due while the app was closed fires once.

A backup check runs on the watcher timer when `watch.backupEnabled` is on (the default), at the interval in `watch.backupIntervalMs` (15 minutes by default, at least 1 minute). It settles outcomes the main pass missed, reports an owned process the system says is gone as Lost, and delivers any message still waiting. For an attached process it checks the PID and birth token again. For a delegated assignment it reads the child’s stored assignment directly. A message sent this way starts with "Backup delivery" and is recorded in History and Diagnostics. It also reminds you about a probe that has not run for twice its interval.

Use `watch.list` and `watch.inspect` to see a watcher's state, elapsed time and current output tail. `watch.cancel` with `stopProcess` stops the launched process and reports Cancelled; without it the watcher stops observing and reports nothing, and the process keeps running under its owner.

In the app, the Watchers screen lists every watcher in every project, newest first. The Current tab beside a conversation lists that conversation's watchers, and a feature's detail lists the watchers of the agents working on it. Each row shows the project, feature, work item, assignment and agent that own the watcher, its source, its state, how long it has run, when it last checked its source, and the last lines of its output. **Cancel watcher** stops observing; **Cancel and stop process** also stops what the watcher launched. The lists read again every few seconds while they are shown. Watcher messages appear in the conversation that owns the watcher, including messages a backup delivery found.

Agents and add-ons cannot write watcher messages themselves. A read-only reviewer cannot start or stop a process through a watcher, and the shell-tools setting turns watchers off with the other process tools. See [Resources](help:resources), [Agents](help:agents) and [Permissions](help:permissions).
