#!/usr/bin/env python3
"""Generate the tap formula from a published release's SHA256SUMS."""

import argparse
from pathlib import Path
import re
from urllib.request import urlopen


TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)
RELEASES = "https://github.com/SerenitySoftware/deliveryboy/releases/download"
DEFAULT_OUTPUT = Path(__file__).resolve().parents[2] / "packaging/homebrew/deliveryboy.rb"


def render(version, sums):
    """Validate every required checksum before producing any output."""
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("version must be a stable version such as 0.4.0")
    checksums = {}
    for line in sums.splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *](\S+)", line)
        if not match:
            raise ValueError(f"invalid SHA256SUMS line: {line!r}")
        checksum, filename = match.groups()
        if filename in checksums:
            raise ValueError(f"duplicate checksum: {filename}")
        checksums[filename] = checksum.lower()
    for target in TARGETS:
        filename = f"deliveryboy-{version}-{target}.tar.gz"
        if filename not in checksums:
            raise ValueError(f"missing checksum: {filename}")

    lines = [
        "class Deliveryboy < Formula",
        '  desc "Release CLI driven by your repository config"',
        '  homepage "https://deliveryboy.app/"',
        '  license "MIT"',
    ]
    for os_name, targets in (("macos", TARGETS[:2]), ("linux", TARGETS[2:])):
        lines.extend(["", f"  on_{os_name} do"])
        for index, (arch, target) in enumerate(zip(("arm", "intel"), targets)):
            filename = f"deliveryboy-{version}-{target}.tar.gz"
            if index:
                lines.append("")
            lines.extend([
                f"    on_{arch} do",
                f'      url "{RELEASES}/v{version}/{filename}"',
                f'      sha256 "{checksums[filename]}"',
                "    end",
            ])
        lines.append("  end")
    lines.extend([
        "",
        "  def install",
        '    bin.install "deliver"',
        "  end",
        "",
        "  test do",
        '    assert_equal "deliver #{version}", shell_output("#{bin}/deliver --version").strip',
        "  end",
        "end",
        "",
    ])
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="release version, with or without a leading v")
    parser.add_argument("--sha256sums", type=Path, help="use an already downloaded SHA256SUMS")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    args = parser.parse_args()
    version = args.version.removeprefix("v")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        parser.error("version must be a stable version such as 0.4.0")
    if args.sha256sums:
        sums = args.sha256sums.read_text()
    else:
        with urlopen(f"{RELEASES}/v{version}/SHA256SUMS", timeout=30) as response:
            sums = response.read().decode("utf-8")
    try:
        formula = render(version, sums)
    except ValueError as error:
        parser.error(str(error))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(formula)
    print(f"Wrote {args.output} for v{version}")


if __name__ == "__main__":
    main()
