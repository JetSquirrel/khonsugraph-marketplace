#!/usr/bin/env python3
"""Build steps for this marketplace: what to release, packing it, listing it.

    marketplace.py plan                      which plugins have a version not released yet
    marketplace.py check                     every manifest is usable and agrees with its Cargo.toml
    marketplace.py guard BASE                a plugin changed since BASE has a version not released yet
    marketplace.py pack PLUGIN --target T [--bin FILE] --out DIR
    marketplace.py verify DIR --base-url URL the packages in DIR are served at URL, byte for byte
    marketplace.py index DIR --base-url URL  list the packages in DIR in marketplace.json

A plugin is released once per version, as the tag `<id>-v<version>`. Nothing
is built for a plugin whose version marketplace.json already lists, so a push
that touches one plugin builds one plugin, and to release a change is to raise
its version.
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import urllib.request
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PLUGINS = ROOT / "plugins"
INDEX = ROOT / "marketplace.json"
MANIFEST = "khonsu-plugin.json"
# Where a compiled plugin is built, as (runner, package target).
PLATFORMS = [("macos-latest", "macos-aarch64"), ("windows-latest", "windows-x86_64")]
# Left out of a script plugin's package.
SKIPPED = {"tests", "__pycache__", "README.md", "Cargo.toml", "src", "target"}


def manifests():
    for path in sorted(PLUGINS.glob(f"*/{MANIFEST}")):
        yield path.parent, json.loads(path.read_text())


def compiled(folder: Path) -> bool:
    return (folder / "Cargo.toml").is_file()


def tag(manifest) -> str:
    return f"{manifest['id']}-v{manifest['version']}"


def released_tags() -> set[str]:
    """What marketplace.json lists, as tags. A version counts as released once
    it is listed, which is the last step: a release that failed partway is not
    listed, so the next run makes it again."""
    listed = json.loads(INDEX.read_text()).get("plugins", [])
    return {f"{entry['id']}-v{entry['version']}" for entry in listed if entry.get("packages")}


def check() -> int:
    problems = []
    for folder, manifest in manifests():
        name = folder.name
        for key in ("id", "name", "version", "protocol", "entry"):
            if not manifest.get(key):
                problems.append(f"{name}: {MANIFEST} has no {key}")
        if manifest.get("id") != name:
            problems.append(f"{name}: its id is {manifest.get('id')!r}, and the folder is {name!r}")
        if not re.fullmatch(r"\d+\.\d+\.\d+", str(manifest.get("version", ""))):
            problems.append(f"{name}: version {manifest.get('version')!r} is not like 1.2.3")
        if compiled(folder):
            cargo = (folder / "Cargo.toml").read_text()
            found = re.search(r'^version\s*=\s*"([^"]+)"', cargo, re.M)
            if not found or found.group(1) != manifest.get("version"):
                problems.append(f"{name}: Cargo.toml is at {found.group(1) if found else None}, and {MANIFEST} at {manifest.get('version')}")
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


def plan() -> int:
    done = released_tags()
    matrix, plugins = [], []
    for folder, manifest in manifests():
        if tag(manifest) in done:
            continue
        plugins.append({"id": manifest["id"], "version": manifest["version"], "tag": tag(manifest)})
        platforms = PLATFORMS if compiled(folder) else [("ubuntu-latest", "any")]
        for runner, target in platforms:
            matrix.append({"plugin": manifest["id"], "os": runner, "target": target, "compiled": compiled(folder), "bin": manifest["entry"][0]})
    lines = [f"matrix={json.dumps(matrix)}", f"plugins={json.dumps(plugins)}", f"any={'true' if matrix else 'false'}"]
    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a") as file:
            file.write("\n".join(lines) + "\n")
    print("\n".join(lines))
    return 0


def guard(base: str) -> int:
    """A plugin whose files changed since `base` must be at a version that has no release."""
    changed = subprocess.run(["git", "diff", "--name-only", f"{base}...HEAD"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.split()
    done = released_tags()
    failed = False
    for folder, manifest in manifests():
        prefix = f"plugins/{folder.name}/"
        touched = [path for path in changed if path.startswith(prefix) and not path.endswith(".md")]
        if touched and tag(manifest) in done:
            print(f"{folder.name}: changed ({touched[0]}, ...) and still at {manifest['version']}, which is released. Raise its version.", file=sys.stderr)
            failed = True
    return 1 if failed else 0


def files_of(folder: Path, binary: Path | None):
    """(name in the package, source file, executable?) for everything a package holds."""
    files = [(MANIFEST, folder / MANIFEST, False)]
    if binary:
        files.append((binary.name, binary, True))
        if (folder / "templates").is_dir():
            files += [(p.relative_to(folder).as_posix(), p, False) for p in sorted((folder / "templates").rglob("*")) if p.is_file()]
        return files
    for path in sorted(folder.rglob("*")):
        relative = path.relative_to(folder)
        skip = any(part in SKIPPED or part.startswith(".") for part in relative.parts) or path.suffix == ".pyc"
        if path.is_file() and not skip and relative.as_posix() != MANIFEST:
            files.append((relative.as_posix(), path, os.access(path, os.X_OK)))
    return files


def pack(plugin: str, target: str, binary: str | None, out: str) -> int:
    folder = PLUGINS / plugin
    manifest = json.loads((folder / MANIFEST).read_text())
    if compiled(folder) and not binary:
        print(f"{plugin} is compiled: give its executable with --bin", file=sys.stderr)
        return 1
    dist = Path(out)
    dist.mkdir(parents=True, exist_ok=True)
    name = f"{manifest['id']}-{manifest['version']}-{target}.zip"
    with zipfile.ZipFile(dist / name, "w", zipfile.ZIP_DEFLATED) as archive:
        for inside, source, executable in files_of(folder, Path(binary) if binary else None):
            # A fixed date, so the same build packs to the same bytes.
            info = zipfile.ZipInfo(inside, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (0o755 if executable else 0o644) << 16
            archive.writestr(info, source.read_bytes())
    data = (dist / name).read_bytes()
    listing = {
        "id": manifest["id"],
        "name": manifest["name"],
        "version": manifest["version"],
        "description": manifest.get("description", ""),
        "permissions": manifest.get("permissions", {}),
        "package": {"target": target, "file": name, "sha256": hashlib.sha256(data).hexdigest(), "size": len(data)},
    }
    (dist / f"{name}.json").write_text(json.dumps(listing, indent=2) + "\n")
    print(f"{dist / name}: {len(data)} bytes, sha256 {listing['package']['sha256']}")
    return 0


def index(dist: str, base_url: str) -> int:
    """Replaces the entry of each plugin packed in `dist`. Other plugins stay as they are."""
    current = json.loads(INDEX.read_text())
    entries = {entry["id"]: entry for entry in current.get("plugins", [])}
    fresh: dict[str, dict] = {}
    for path in sorted(Path(dist).glob("*.zip.json")):
        listing = json.loads(path.read_text())
        package = listing.pop("package")
        entry = fresh.setdefault(listing["id"], {**listing, "packages": []})
        release = f"{listing['id']}-v{listing['version']}"
        entry["packages"].append({
            "target": package["target"],
            "url": base_url.rstrip("/").format(tag=release) + "/" + package["file"],
            "sha256": package["sha256"],
            "size": package["size"],
        })
    for entry in fresh.values():
        entry["packages"].sort(key=lambda package: package["target"])
        entries[entry["id"]] = entry
    # A plugin taken out of the repository is taken out of the index.
    here = {manifest["id"] for _, manifest in manifests()}
    current["plugins"] = [entries[key] for key in sorted(entries) if key in here]
    INDEX.write_text(json.dumps(current, indent=2) + "\n")
    print(f"{INDEX.name}: listed {', '.join(sorted(fresh)) or 'nothing new'}")
    return 0


def verify(dist: str, base_url: str) -> int:
    """Each package in `dist` can be downloaded from where the index will say it is, and is the same bytes."""
    failed = False
    for path in sorted(Path(dist).glob("*.zip.json")):
        listing = json.loads(path.read_text())
        package = listing["package"]
        release = f"{listing['id']}-v{listing['version']}"
        url = base_url.rstrip("/").format(tag=release) + "/" + package["file"]
        try:
            request = urllib.request.Request(url, headers={"User-Agent": "khonsugraph-marketplace"})
            with urllib.request.urlopen(request, timeout=120) as response:
                found = hashlib.sha256(response.read()).hexdigest()
        except Exception as error:  # noqa: BLE001 - whatever went wrong, the package is not there
            print(f"{url}: {error}", file=sys.stderr)
            failed = True
            continue
        if found != package["sha256"]:
            print(f"{url}: sha256 {found}, and {package['sha256']} was packed", file=sys.stderr)
            failed = True
        else:
            print(f"{url}: ok")
    return 1 if failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("plan")
    commands.add_parser("check")
    commands.add_parser("guard").add_argument("base")
    packing = commands.add_parser("pack")
    packing.add_argument("plugin")
    packing.add_argument("--target", required=True)
    packing.add_argument("--bin")
    packing.add_argument("--out", default="dist")
    listing = commands.add_parser("index")
    listing.add_argument("dist")
    listing.add_argument("--base-url", required=True, help="where packages are served; {tag} is the release, <id>-v<version>")
    verifying = commands.add_parser("verify")
    verifying.add_argument("dist")
    verifying.add_argument("--base-url", required=True)
    args = parser.parse_args()
    if args.command == "verify":
        return verify(args.dist, args.base_url)
    if args.command == "plan":
        return plan()
    if args.command == "check":
        return check()
    if args.command == "guard":
        return guard(args.base)
    if args.command == "pack":
        return pack(args.plugin, args.target, args.bin, args.out)
    return index(args.dist, args.base_url)


if __name__ == "__main__":
    sys.exit(main())
