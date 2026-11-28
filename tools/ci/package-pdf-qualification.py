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
import zipfile


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def dependency_notices(target: str, notices: Path) -> None:
    """Package declared license terms and supplied notices for the target's runtime graph."""
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target],
        text=True,
    ))
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    roots = {("lopdf", "0.47.0"), ("hayro", "0.9.0"), ("image", "0.25.10")}
    pending = [package["id"] for package in packages.values()
               if (package["name"], package["version"]) in roots]
    if len(pending) != len(roots):
        raise ValueError("PDF dependency roots diverged from the approved locks")
    visited = set()
    inventory = []
    while pending:
        package_id = pending.pop()
        if package_id in visited:
            continue
        visited.add(package_id)
        pending.extend(dep["pkg"] for dep in nodes[package_id]["deps"]
                       if any(kind["kind"] is None for kind in dep["dep_kinds"]))
        package = packages[package_id]
        if package["source"] is None:
            continue
        if not package["license"] and not package["license_file"]:
            raise ValueError(f"missing declared dependency license for {package['name']}")
        root = Path(package["manifest_path"]).parent.resolve()
        terms = [path for path in root.rglob("*")
                 if path.is_file() and not path.is_symlink()
                 and path.name.lower().startswith(("license", "copying", "copyright", "notice"))]
        if package["license_file"]:
            licensed_file = (root / package["license_file"]).resolve()
            if licensed_file.is_file() and licensed_file.is_relative_to(root.resolve()):
                terms.append(licensed_file)
        local = notices / "dependencies" / f"{package['name']}-{package['version']}"
        files = []
        for term in sorted(set(terms)):
            relative = term.relative_to(root)
            destination = local / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(term, destination)
            files.append(str(destination.relative_to(notices)))
        if not files and (package["name"], package["version"]) == ("alloc-stdlib", "0.2.4"):
            files.append("LICENSE-alloc-stdlib-0.2.4")
        if not files:
            raise ValueError(f"missing source notices for {package['name']} {package['version']}")
        inventory.append({
            "crate": package["name"],
            "version": package["version"],
            "source": package["source"],
            "license": package["license"],
            "notices": files,
        })
    inventory.sort(key=lambda item: (item["crate"], item["version"]))
    (notices / "dependencies.json").write_text(json.dumps(
        {"target": target, "scope": "conservative Cargo PDF and raster runtime dependency graph", "crates": inventory},
        indent=2,
    ) + "\n", encoding="utf-8")


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
    dependency_notices(target, notices)
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
    elif windows:
        archive = archive_name.with_suffix(".zip")
        # Cargo registry archives can preserve Unix-epoch notice times. ZIP stores a DOS date.
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED, strict_timestamps=False) as bundle:
            for member in sorted(package.rglob("*")):
                if member.is_file():
                    bundle.write(member, member.relative_to(package))
        shutil.unpack_archive(archive, installed)
    else:
        archive = Path(shutil.make_archive(str(archive_name), "gztar", package))
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
        "dependencyInventorySha256": digest(notices / "dependencies.json"),
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
