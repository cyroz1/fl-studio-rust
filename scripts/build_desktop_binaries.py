#!/usr/bin/env python3
"""Build the desktop application and the isolated VST3 host helper."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", choices=("debug", "release"), default="release")
    args = parser.parse_args()

    root = Path(__file__).resolve().parent.parent
    cargo_profile = ["--release"] if args.profile == "release" else []
    subprocess.run(
        ["cargo", "build", *cargo_profile, "--bin", "fl-studio-rebuild"],
        cwd=root,
        check=True,
    )

    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1"],
            cwd=root,
            text=True,
            encoding="utf-8",
        )
    )
    root_manifest = (root / "Cargo.toml").resolve()
    root_package = next(
        package
        for package in metadata["packages"]
        if Path(package["manifest_path"]).resolve() == root_manifest
    )
    root_node = next(
        node for node in metadata["resolve"]["nodes"] if node["id"] == root_package["id"]
    )
    host_dependency_ids = {
        dependency["pkg"]
        for dependency in root_node["deps"]
        if dependency["name"].replace("_", "-") == "vst3-host"
    }
    helper_packages = [
        package
        for package in metadata["packages"]
        if package["name"] == "vst3-host" and package["id"] in host_dependency_ids
    ]
    if len(helper_packages) != 1:
        raise SystemExit("cargo metadata did not resolve one direct vst3-host dependency")
    helper_package = helper_packages[0]

    build_env = os.environ.copy()
    build_env["CARGO_TARGET_DIR"] = metadata["target_directory"]
    subprocess.run(
        [
            "cargo",
            "build",
            *cargo_profile,
            "--manifest-path",
            helper_package["manifest_path"],
            "--bin",
            "vst3-host-helper",
            "--no-default-features",
            "--features",
            "process-isolation",
        ],
        cwd=root,
        env=build_env,
        check=True,
    )

    helper_name = "vst3-host-helper.exe" if os.name == "nt" else "vst3-host-helper"
    helper_path = Path(metadata["target_directory"]) / args.profile / helper_name
    if not helper_path.is_file():
        raise SystemExit(f"VST3 host helper was not built at {helper_path}")
    print(f"Built isolated VST3 helper at {helper_path}")


if __name__ == "__main__":
    main()
