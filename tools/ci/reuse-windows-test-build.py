"""Relay the two encrypted transfers from the verified DW-416 build attempt."""

import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import zipfile

REPOSITORY = "SceneWorks/DevWorks"
REPO_API = f"repos/{REPOSITORY}"
BUILD_RUN = 45038426993
BUILD_JOB = 137257290112
WORKFLOW_ID = 426880357
WORKFLOW_REVISION = "46ece2fabcb8ee47b2d77956f440e09175891bd6"
CANDIDATE = "1ae1be523bd7202900904fb9d1b173d1caa07996"
TRANSFERS = (
    (
        14762612404,
        "dw416-unsigned-1",
        "unsigned.7z",
        "aabfe5ae485244adbaeeddf6d33a6ebc022bbbc42d0b3409a0efa8e0f92c5fba",
    ),
    (
        14762707171,
        "dw416-tests-1",
        "tests.7z",
        "ff7b2d3ef8d38984beb22a521009ff79b48ce997eeee8beaa2739fa21c076e18",
    ),
)


def github(path: str) -> dict:
    return json.loads(subprocess.check_output(["gh", "api", f"{REPO_API}/{path}"]))


def verify_build(source: str) -> dict:
    if source != CANDIDATE:
        raise ValueError("This recovery is bound to the accepted DW-414 candidate")
    run = github(f"actions/runs/{BUILD_RUN}")
    if (
        run["repository"]["full_name"] != REPOSITORY
        or run["event"] != "workflow_dispatch"
        or run["head_branch"] != "main"
        or run["head_sha"] != WORKFLOW_REVISION
        or run["workflow_id"] != WORKFLOW_ID
        or run["path"] != ".github/workflows/windows-test-installer.yml"
        or run["run_attempt"] != 1
        or run["status"] != "completed"
    ):
        raise ValueError("The original protected workflow identity changed")
    jobs = github(f"actions/runs/{BUILD_RUN}/attempts/1/jobs?per_page=100")["jobs"]
    build = [job for job in jobs if job["id"] == BUILD_JOB]
    if len(build) != 1 or build[0]["conclusion"] != "success":
        raise ValueError("The exact native build job did not complete successfully")
    artifacts = github(f"actions/runs/{BUILD_RUN}/artifacts?per_page=100")["artifacts"]
    return {item["id"]: item for item in artifacts}


def archive_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def relay(source: str, destination: Path) -> None:
    artifacts = verify_build(source)
    print(f"Recovering verified build job {BUILD_JOB} from run {BUILD_RUN}, attempt 1.", flush=True)
    with tempfile.TemporaryDirectory(prefix="verified-build-", dir=destination) as scratch:
        for artifact_id, name, member, digest in TRANSFERS:
            metadata = artifacts.get(artifact_id)
            if (
                not metadata
                or metadata["name"] != name
                or metadata["expired"]
                or metadata["digest"] != f"sha256:{digest}"
            ):
                raise ValueError(f"The exact recovery artifact {artifact_id} is unavailable")
            archive_path = Path(scratch) / "transfer.zip"
            with archive_path.open("wb") as archive:
                subprocess.run(
                    ["gh", "api", f"{REPO_API}/actions/artifacts/{artifact_id}/zip"],
                    check=True,
                    stdout=archive,
                )
            if archive_digest(archive_path) != digest:
                raise ValueError(f"The original ZIP digest does not match for {artifact_id}")
            with zipfile.ZipFile(archive_path) as archive:
                if archive.namelist() != [member]:
                    raise ValueError(f"Unexpected members in encrypted artifact {artifact_id}")
                with archive.open(member) as contents, (destination / member).open("xb") as output:
                    shutil.copyfileobj(contents, output)
            print(f"Relayed original artifact {artifact_id} with checked ZIP SHA-256 {digest}.", flush=True)


if __name__ == "__main__":
    relay(sys.argv[1], Path(sys.argv[2]).resolve(strict=True))
