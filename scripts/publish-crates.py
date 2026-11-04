#!/usr/bin/env python3
"""Validate and publish the msb_krun release train in dependency order."""

import argparse
import graphlib
import json
import os
from pathlib import Path
import re
import subprocess
import time
import urllib.error
import urllib.request


def release_order(metadata, version):
    if not re.fullmatch(r"0\.[0-9]+\.[0-9]+", version):
        raise ValueError("expected a stable 0.x.y release version")
    packages = {
        p["name"]: p for p in metadata["packages"]
        if p["name"].startswith("msb_krun")
    }
    if not packages:
        raise ValueError("no msb_krun release crates found")
    graph = {}
    for name, package in packages.items():
        if package["version"] != version or package.get("publish") == []:
            raise ValueError(f"{name} is not publishable at {version}")
        dependencies = set()
        for dependency in package["dependencies"]:
            target = dependency["name"]
            if not target.startswith("msb_krun"):
                continue
            if target not in packages or dependency["req"] != f"^{version}":
                raise ValueError(f"{name}: inconsistent release dependency {target}")
            # Include optional and target-specific dependencies: crates.io requires
            # those versions too, regardless of the publishing runner's platform.
            if dependency["kind"] != "dev":
                dependencies.add(target)
        graph[name] = dependencies
    return [packages[name] for name in graphlib.TopologicalSorter(graph).static_order()]


def published(name, version):
    # All release crate names exceed three characters (Cargo sparse-index layout).
    url = f"https://index.crates.io/{name[:2]}/{name[2:4]}/{name}"
    request = urllib.request.Request(url, headers={"User-Agent": "libkrun-release"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            entries = response.read().decode().splitlines()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return False
        raise  # Authentication, rate limits and outages are never 'not published'.
    for line in entries:
        entry = json.loads(line)
        if entry["vers"] == version:
            if entry.get("yanked", False):
                raise ValueError(f"{name} {version} is yanked; refusing to continue")
            return True
    return False


def publish(packages, version):
    if not os.environ.get("CARGO_REGISTRY_TOKEN"):
        raise ValueError("configure CARGO_REGISTRY_TOKEN in the crates-io environment")
    for package in packages:
        name = package["name"]
        if published(name, version):
            print(f"Already published: {name} {version}", flush=True)
            continue
        print(f"Publishing {name} {version}", flush=True)
        # Platform-specific crates cannot all be compiled on one runner. Build
        # verification belongs to the PR's Linux, macOS and Windows CI jobs.
        subprocess.run([
            "cargo", "publish", "--registry", "crates-io", "--no-verify",
            "--manifest-path", package["manifest_path"],
        ], check=True)
        deadline = time.monotonic() + 300
        while not published(name, version):
            if time.monotonic() >= deadline:
                raise TimeoutError(f"{name} {version} is not visible in the index; rerun later")
            time.sleep(5)
        print(f"Available: {name} {version}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--publish", action="store_true", help="upload; default only validates the plan")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    os.chdir(root)
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--no-deps", "--format-version", "1",
    ], text=True))
    packages = release_order(metadata, args.version)
    print(f"Release {args.version}: " + " -> ".join(p["name"] for p in packages), flush=True)
    if args.publish:
        publish(packages, args.version)


if __name__ == "__main__":
    main()
