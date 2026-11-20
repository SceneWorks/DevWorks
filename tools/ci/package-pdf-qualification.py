"""Stage and attest the native PDF worker in a pre-release desktop package."""
import argparse
import hashlib
import json
import os
import platform
from pathlib import Path
import plistlib
import shutil
import subprocess


def digest(path: Path) -> str:
    with path.open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def stage(binaries: Path, output: Path, target: str) -> Path:
    system = platform.system()
    windows = system == "Windows"
    macos = system == "Darwin"
    package = output / "stage"
    if package.exists():
        raise ValueError(f"qualification staging folder already exists: {package}")
    destination = package
    if macos:
        application = package / "DevWorks.app"
        destination = application / "Contents" / "MacOS"
    destination.mkdir(parents=True)
    names = [name + (".exe" if windows else "") for name in ("devworks-desktop", "devworks-pdf-worker")]
    for name in names:
        shutil.copy2(binaries / name, destination / name)
    notices = (application / "Contents" / "Resources" if macos else package) / "notices"
    shutil.copytree(Path("packaging/pdf"), notices)
    shutil.copy2("Cargo.lock", notices)
    if macos:
        with (application / "Contents" / "Info.plist").open("wb") as handle:
            plistlib.dump(
                {
                    "CFBundleExecutable": names[0],
                    "CFBundleIdentifier": "com.sceneworks.devworks.pdf-qualification",
                    "CFBundleName": "DevWorks",
                    "CFBundlePackageType": "APPL",
                    "CFBundleVersion": "1",
                },
                handle,
            )
        for member in [destination / names[1], application]:
            subprocess.run(["codesign", "--force", "--sign", "-", member], check=True)
        subprocess.run(["codesign", "--verify", "--deep", "--strict", application], check=True)
    installed = output / "installed"
    installed.mkdir()
    archive_name = output / f"devworks-native-pdf-{target}"
    if macos:
        archive = archive_name.with_suffix(".zip")
        subprocess.run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", application, archive], check=True)
        subprocess.run(["ditto", "-x", "-k", archive, installed], check=True)
        subprocess.run(["codesign", "--verify", "--deep", "--strict", installed / application.name], check=True)
    else:
        archive = Path(shutil.make_archive(str(archive_name), "zip" if windows else "gztar", package))
        shutil.unpack_archive(archive, installed)
    destination = installed / destination.relative_to(package)
    notices = installed / notices.relative_to(package)
    subprocess.run([destination / names[1], "--check-pdf-worker-memory"], check=True)
    evidence = {
        "target": target,
        "platform": platform.platform(),
        "macQualificationSigning": "ad-hoc" if macos else None,
        "desktop": {
            "path": str((destination / names[0]).relative_to(installed)),
            "bytes": (destination / names[0]).stat().st_size,
            "sha256": digest(destination / names[0]),
        },
        "worker": {
            "path": str((destination / names[1]).relative_to(installed)),
            "bytes": (destination / names[1]).stat().st_size,
            "sha256": digest(destination / names[1]),
        },
        "archiveBytes": archive.stat().st_size,
        "archiveSha256": digest(archive),
        "lockSha256": digest(notices / "Cargo.lock"),
    }
    report = json.dumps(evidence, indent=2)
    (output / "qualification.json").write_text(report + "\n", encoding="utf-8")
    print(report)
    desktop = destination / names[0]
    if "GITHUB_ENV" in os.environ:
        with open(os.environ["GITHUB_ENV"], "a", encoding="utf-8") as handle:
            handle.write(f"DEVWORKS_PDF_PACKAGED_DESKTOP={desktop}\n")
    return desktop


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target", required=True)
    args = parser.parse_args()
    stage(args.binaries.resolve(), args.output.resolve(), args.target)
