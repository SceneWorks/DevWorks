# Architecture feasibility experiments

Disposable native Rust experiments, not production application scaffolding. The pinned lockfile covers matching egui/eframe 0.37.0 with native AccessKit/Glow and macOS security-framework 3.7.0. No inference, provider sign-in or external effects run.

From the repository root:

```sh
CARGO_TARGET_DIR="$PWD/.planning-build/architecture-gates/target" cargo build --locked -j 4 --manifest-path tools/architecture-gates-eval/Cargo.toml
.planning-build/architecture-gates/target/debug/devworks-architecture-gates-probe secret
.planning-build/architecture-gates/target/debug/devworks-architecture-gates-probe process
.planning-build/architecture-gates/target/debug/devworks-architecture-gates-probe ui
```

`secret` creates a PID-qualified synthetic Keychain item, reads/rotates/deletes it and checks absence, with Keychain UI disabled. It never enumerates or accesses existing secrets. `process` creates an owned group with a cooperative child/descendant, sends group cancellation and waits for reaping. Its finite-lifetime fixture is not hostile-process containment. `ui` creates an invisible native window, runs two frames and closes it; it does not establish OS screen-reader behavior. Use an outer 25-second process timeout when reproducing.

These modes require macOS/native window-session support for the recorded results. Windows/Linux implementation acceptance, signed identity/ACLs, locked stores, escaped children, complete panes, variable-height streaming and assistive technology remain unexecuted. See the [evidence and limits](../../docs/architecture/validation/architecture-gates/README.md).
