"""Exercise signed Windows updater builds in a disposable, native runner profile."""

import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import time

ENTRY = "devworks-desktop.exe"
FILES = (ENTRY, "devworks-pdf-worker.exe")
APPROVE = "DW417_QUALIFICATION_APPROVE"
SCREENSHOT = "DW417_QUALIFICATION_SCREENSHOT"
STARTUP_ERROR = "DW417_QUALIFICATION_STARTUP_ERROR"


def prepare_source(checkout: Path) -> None:
    """Patch only the source archive used to build the qualification executables."""
    if (checkout / ".git").exists():
        raise ValueError("prepare requires a disposable source archive")
    path = checkout / "crates/devworks-desktop/src/main.rs"
    source = path.read_text(encoding="utf-8")
    marker = '    let screenshot = arg("--screenshot").map(PathBuf::from);'
    if source.count(marker) != 1:
        raise ValueError("the desktop screenshot entry point changed")
    source = source.replace(marker, """    let close_at = close_at.or_else(|| {
        candidate.as_ref()?;
        Some(Instant::now() + Duration::from_secs(25))
    });
    let screenshot = arg("--screenshot").map(PathBuf::from).or_else(|| {
        candidate.as_ref()?;
        std::env::var_os("DW417_QUALIFICATION_SCREENSHOT").map(PathBuf::from)
    });""")
    marker = "    let startup = app_updates::enter_application();"
    if source.count(marker) != 1:
        raise ValueError("the candidate admission entry point changed")
    source = source.replace(marker, marker + '''
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--internal-app-startup"))
        && let Err(error) = &startup
        && let Some(path) = std::env::var_os("DW417_QUALIFICATION_STARTUP_ERROR")
    {
        std::fs::write(path, error.as_bytes()).expect("record the actual candidate admission error");
    }
''')
    marker = "                    Ok(engine) => {"
    if source.count(marker) != 1:
        raise ValueError("the desktop update gateway entry point changed")
    source = source.replace(marker, marker + """
                        if candidate.is_none() && std::env::var_os("DW417_QUALIFICATION_APPROVE").is_some() {
                            use devworks_core::operations::{OperationInput, workspace};
                            use devworks_desktop::frame::{Negotiate, build};
                            let backend = engine.backend();
                            let selected = backend.update_status().unwrap().current.unwrap();
                            assert_eq!(selected.proposal_id.as_str(), "fixture-approval");
                            let input = workspace::ApproveInstallInput {
                                update_id: selected.record.id,
                                expected_revision: selected.record.revision,
                                binary_digest: selected.record.digest,
                                proposal_id: selected.proposal_id,
                            };
                            let frame = build(
                                &*backend,
                                Negotiate::Initial,
                                Some(OperationInput::Workspace(workspace::Input::ApproveInstall(input.clone()))),
                            );
                            backend.approve_app_update(&frame, &input.proposal_id, backend.control_epoch())
                                .expect("the native update gateway must approve the exact fixture");
                        }
""")
    path.write_text(source, encoding="utf-8")


def packed(value) -> bytes:
    return json.dumps(value, separators=(",", ":")).encode("utf-8")


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        result = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
        return result.hexdigest()


def wait_for(description, condition, timeout=180):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            result = condition()
        except (sqlite3.OperationalError, OSError, json.JSONDecodeError):
            result = None
        if result:
            return result
        time.sleep(0.2)
    raise AssertionError(f"Timed out waiting for {description}")


def query(db, sql):
    with closing(sqlite3.connect(db.resolve().as_uri() + "?mode=ro", uri=True, timeout=1)) as conn:
        return conn.execute(sql).fetchone()


def clean_boot_count(db):
    return query(db, "SELECT count(*) FROM app_boots WHERE shutdown_state = 'Clean'")[0]


def locks_are_available(control, profile):
    import msvcrt

    # Windows' process and install gates both cover byte zero. The clean SQLite
    # receipt alone can appear while one of those processes still owns a gate.
    for path in [control / "installation.lock", profile / "workspace.lock"]:
        with path.open("r+b") as lock:
            msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
            msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
    return True


def state(db):
    row = query(db, "SELECT record_json FROM app_update_state WHERE singleton = 1")
    return json.loads(row[0]) if row else {}


def evidence(image):
    return json.loads(subprocess.check_output([image, "--release-evidence"], timeout=20))


def signed_stage(profile, image, release, key, mode):
    candidate = profile / "app-update-stages"
    candidate.mkdir(exist_ok=True)
    staging = candidate / "qualification-pending"
    staging.mkdir()
    archive = staging / "app.bundle"
    files = []
    with archive.open("xb") as output:
        for name in sorted(FILES):
            path = image / name
            files.append({
                "path": name,
                "executable": True,
                "content": {"bytes": path.stat().st_size, "sha256": digest(path)},
            })
            with path.open("rb") as member:
                shutil.copyfileobj(member, output)
    build = release["build"]
    if mode == "pre-admission-rollback":
        build = ("1" if build.startswith("0") else "0") + build[1:]
    target = release["target"]
    manifest = {
        "format": 1,
        "product": "SceneWorks/DevWorks",
        "releaseId": 417,
        "tag": "v" + release["version"],
        "build": build,
        "target": target,
        "owner": "windows_per_user",
        "archive": {"bytes": archive.stat().st_size, "sha256": digest(archive)},
        "archiveAsset": f"DevWorks-{target}.bundle",
        "helpDigest": release["helpDigest"],
        "migrations": release["migrations"],
        "files": files,
        "destructiveMigration": False,
    }
    document = staging / "manifest.json"
    document.write_bytes(packed(manifest))
    signature = staging / "manifest.sig"
    subprocess.run(
        ["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", document, "-out", signature],
        check=True,
        timeout=20,
    )
    assert signature.stat().st_size == 64
    commitment = digest(document)
    if mode == "changed-worker":
        # The desktop sorts first. Change only the worker after sealing the archive.
        worker_offset = files[0]["content"]["bytes"] + 64
        with archive.open("r+b") as writable:
            writable.seek(worker_offset)
            original = writable.read(1)
            assert len(original) == 1
            writable.seek(worker_offset)
            writable.write(bytes([original[0] ^ 1]))
    staging.rename(candidate / commitment)
    return manifest, commitment


def seed_approval(db, manifest, commitment):
    with closing(sqlite3.connect(db, timeout=5)) as conn, conn:
        persisted = json.loads(
            conn.execute("SELECT record_json FROM app_update_state WHERE singleton=1").fetchone()[0]
        )
        assert persisted["approval"] is None
        now = int(time.time() * 1000)
        binary = next(file["content"]["sha256"] for file in manifest["files"] if file["path"] == ENTRY)
        persisted.update(checked_at_ms=now, retry_at_ms=0, failures=0, issue=None)
        persisted["update"] = {
            "record": {
                "id": "fixture-update",
                "revision": "1",
                "version": manifest["tag"][1:],
                "channel": "Stable",
                "digest": binary,
                "signatureVerified": True,
                "state": "NeedsInput",
                "migrationRequired": False,
            },
            "stage": {"manifest": manifest, "manifest_digest": commitment},
            "proposal_id": "fixture-approval",
        }
        conn.execute(
            "UPDATE app_update_state SET record_json=? WHERE singleton=1",
            (packed(persisted).decode("utf-8"),),
        )
        conn.execute(
            """INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at)
               VALUES ('preserved-project','Authored test work','Active',1,1,?)""",
            (now,),
        )


def start(launcher, profile, log, env):
    with log.open("wb") as output:
        subprocess.run(
            [launcher, "--data-dir", profile, "--close-after-secs", "6"],
            env=env,
            stdout=output,
            stderr=subprocess.STDOUT,
            check=True,
            timeout=40,
        )


def one_case(case, root, older, newer, key, next_release, wanted):
    case.mkdir()
    profile = case / "profile"
    database = profile / "workspace.sqlite"
    control = root.parent / ".DevWorks-updates"
    if control.exists():
        raise ValueError("existing managed update journal on the qualification runner")
    root.mkdir()
    old_pointer = digest(older / ENTRY)
    versions = root / "versions"
    old = versions / old_pointer
    shutil.copytree(older, old)
    launcher = root / "DevWorks.exe"
    shutil.copyfile(older / ENTRY, launcher)
    current = root / "current"
    current.write_text(old_pointer, encoding="ascii")
    env = os.environ.copy()
    env.pop(APPROVE, None)
    env[SCREENSHOT] = str(case / "candidate.png")
    candidate_error = case / "candidate-error.txt"
    env[STARTUP_ERROR] = str(candidate_error)
    start(launcher, profile, case / "initial.log", env)
    wait_for("a clean initial startup", lambda: clean_boot_count(database) == 1)
    wait_for("the initial app to release both gates", lambda: locks_are_available(control, profile))

    manifest, commitment = signed_stage(profile, newer, next_release, key, case.name)
    seed_approval(database, manifest, commitment)
    start(launcher, profile, case / "approval.log", {**env, APPROVE: "1"})

    def settled():
        transaction = (control / "active-transaction").read_text(encoding="ascii")
        journal = json.loads((control / f"update-{transaction}.json").read_text(encoding="utf-8"))
        return journal if journal["stage"] in {"Healthy", "RolledBack", "RecoveryRequired", "Unchanged"} else None

    journal = wait_for("the native updater journal to settle", settled)
    assert journal["stage"] == wanted, journal.get("detail")
    assert (journal["installer_pid"] is None) == (wanted == "Unchanged")
    final_pointer = commitment if wanted == "Healthy" else old_pointer
    assert current.read_text(encoding="ascii") == final_pointer
    if wanted != "Healthy":
        if wanted == "RolledBack":
            assert candidate_error.read_text(encoding="utf-8") == (
                "The new application build, executable or bundled Help differs from the signed approval."
            )
            assert (versions / commitment / ENTRY).is_file()
        else:
            assert "a workspace archive member changed during copy" in journal["detail"]
            assert not (versions / commitment).exists()
            assert not candidate_error.exists()
        wait_for("the updater to release both gates before reopening", lambda: locks_are_available(control, profile))
        start(launcher, profile, case / "rolled-back.log", env)
        wait_for("the restored installation to boot and close", lambda: clean_boot_count(database) >= 3)
        assert state(database)["last"]["state"] == "Failed"
    else:
        png = case / "candidate.png"
        wait_for("the normal candidate shutdown", lambda: clean_boot_count(database) >= 3)
        frame = png.read_bytes()
        assert frame[:8] == bytes.fromhex("89504e470d0a1a0a")
        assert frame[12:16] == b"IHDR" and frame.endswith(bytes.fromhex("0000000049454e44ae426082"))
        assert state(database)["last"]["state"] == "ObservedComplete"

    assert query(database, "SELECT title FROM projects WHERE id='preserved-project'") == (
        "Authored test work",
    )
    assert digest(launcher) == old_pointer, "the fixed launcher changed during update"
    assert current.read_text(encoding="ascii") == final_pointer
    expected = newer if wanted == "Healthy" else older
    for name in FILES:
        assert digest(versions / final_pointer / name) == digest(expected / name)
    wait_for("native app and updater exit", lambda: locks_are_available(control, profile))

    def cleanup_owned_paths():
        for path in (root, control):
            if path.exists():
                shutil.rmtree(path)
        return True

    wait_for("the qualification executables to leave memory", cleanup_owned_paths, 30)
    print(json.dumps({"case": case.name, "journal": wanted, "preserved": "authored project"}), flush=True)


def exercise(root, local_app_data, key, source):
    if os.name != "nt":
        raise ValueError("native Windows runner required")
    older, newer = root / "older", root / "newer"
    for folder in (older, newer):
        assert {member.name for member in folder.iterdir()} == set(FILES)
    old, new = evidence(older / ENTRY), evidence(newer / ENTRY)
    assert (old["version"], new["version"]) == ("0.1.0", "0.1.1")
    assert old["build"] == new["build"] == source
    assert old["target"] == new["target"] == "x86_64-pc-windows-msvc"
    assert old["migrations"] == new["migrations"]
    programs = local_app_data / "Programs"
    programs.mkdir(exist_ok=True)
    owner_root = programs / "DevWorks"
    for name, stage in [
        ("approved-upgrade", "Healthy"),
        ("pre-admission-rollback", "RolledBack"),
        ("changed-worker", "Unchanged"),
    ]:
        one_case(root / name, owner_root, older, newer, key, new, stage)


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    patch = commands.add_parser("prepare")
    patch.add_argument("checkout", type=Path)
    run = commands.add_parser("exercise")
    run.add_argument("root", type=Path)
    run.add_argument("local_app_data", type=Path)
    run.add_argument("key", type=Path)
    run.add_argument("source")
    args = parser.parse_args()
    if args.command == "prepare":
        prepare_source(args.checkout)
    else:
        exercise(args.root, args.local_app_data, args.key, args.source)


if __name__ == "__main__":
    main()
