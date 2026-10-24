#!/usr/bin/env python3
"""Build the desktop application and the isolated VST3 host helper."""

from __future__ import annotations

import argparse
import json
import os
import platform
from pathlib import Path
import shutil
import subprocess
import sys


def is_x86_32_helper(path: Path, windows: bool) -> bool:
    with path.open("rb") as helper:
        header = helper.read(64)
        if windows and header.startswith(b"MZ") and len(header) >= 64:
            pe_offset = int.from_bytes(header[0x3C:0x40], "little")
            helper.seek(pe_offset)
            pe_header = helper.read(6)
            return pe_header[:4] == b"PE\0\0" and pe_header[4:6] == b"\x4C\x01"
        return (
            not windows
            and header.startswith(b"\x7FELF")
            and header[4] == 1
            and header[5] == 1
            and header[18:20] == b"\x03\x00"
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", choices=("debug", "release"), default="release")
    parser.add_argument(
        "--build-x86-helper",
        action="store_true",
        help="cross-compile and stage the 32-bit x86 VST3 helper for Windows or Linux x64",
    )
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

    if args.build_x86_helper:
        if os.name == "nt":
            target = "i686-pc-windows-msvc"
            helper_x86_name = "vst3-host-helper-x86.exe"
        elif sys.platform == "linux" and platform.machine().lower() in ("x86_64", "amd64"):
            target = "i686-unknown-linux-gnu"
            helper_x86_name = "vst3-host-helper-x86"
        else:
            raise SystemExit("the x86 VST3 helper is supported only in Windows and Linux x64 builds")

        subprocess.run(
            [
                "cargo",
                "build",
                *cargo_profile,
                "--target",
                target,
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
        cross_helper_path = (
            Path(metadata["target_directory"]) / target / args.profile / helper_name
        )
        if not cross_helper_path.is_file():
            raise SystemExit(f"32-bit VST3 helper was not built at {cross_helper_path}")
        if not is_x86_32_helper(cross_helper_path, os.name == "nt"):
            raise SystemExit(f"cross-built VST3 helper is not a 32-bit x86 binary: {cross_helper_path}")
        staged_helper_path = Path(metadata["target_directory"]) / args.profile / helper_x86_name
        shutil.copy2(cross_helper_path, staged_helper_path)
        smoke = subprocess.run(
            [str(staged_helper_path)],
            input="",
            capture_output=True,
            text=True,
            timeout=10,
        )
        if smoke.returncode != 0:
            raise SystemExit(
                f"32-bit VST3 helper could not start on this runner: {smoke.stderr.strip()}"
            )
        print(f"Built 32-bit x86 VST3 helper at {staged_helper_path}")

        packager_path = root / "Packager.toml"
        packager_lines = packager_path.read_text(encoding="utf-8").splitlines()
        binaries_start = next(
            (index for index, line in enumerate(packager_lines) if line.strip() == "binaries = ["),
            None,
        )
        if binaries_start is None:
            raise SystemExit("Packager.toml is missing its binaries array")
        binaries_end = next(
            (
                index
                for index in range(binaries_start + 1, len(packager_lines))
                if packager_lines[index].strip() == "]"
            ),
            None,
        )
        if binaries_end is None:
            raise SystemExit("Packager.toml has an unterminated binaries array")
        packager_lines.insert(binaries_end, f'    {{ path = "{helper_x86_name}" }},')
        (root / "Packager.bridge.toml").write_text(
            "\n".join(packager_lines) + "\n", encoding="utf-8"
        )

        nfpm_path = root / "packaging" / "nfpm.yaml"
        nfpm_config = nfpm_path.read_text(encoding="utf-8")
        native_entry = (
            "  - src: target/release/vst3-host-helper\n"
            "    dst: /usr/bin/vst3-host-helper\n"
            "    file_info:\n"
            "      mode: 0o755\n"
        )
        x86_entry = (
            f"  - src: target/release/{helper_x86_name}\n"
            f"    dst: /usr/bin/{helper_x86_name}\n"
            "    file_info:\n"
            "      mode: 0o755\n"
        )
        if nfpm_config.count(native_entry) != 1:
            raise SystemExit("nfpm.yaml is missing its native VST3 helper entry")
        for dependency, marker in [
            ("libc6-i386", "      - libxkbcommon-x11-0\n"),
            ("glibc.i686", "      - \"libxkbcommon-x11.so.0()(64bit)\"\n"),
            ("lib32-glibc", "      - libxkbcommon-x11\n"),
        ]:
            if nfpm_config.count(marker) != 1:
                raise SystemExit(f"nfpm.yaml is missing the {dependency} dependency insertion point")
            nfpm_config = nfpm_config.replace(marker, marker + f"      - {dependency}\n", 1)
        (root / "packaging" / "nfpm-bridge.yaml").write_text(
            nfpm_config.replace(native_entry, native_entry + x86_entry, 1),
            encoding="utf-8",
        )


if __name__ == "__main__":
    main()
