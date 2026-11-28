#!/usr/bin/env python3
"""Check release updates and reject checksum files that could break a tap."""

import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "update_homebrew", Path(__file__).with_name("update-homebrew.py")
)
generator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(generator)


class FormulaUpdateTest(unittest.TestCase):
    def sums(self, version="1.2.3"):
        return "\n".join(
            f"{str(index) * 64}  deliveryboy-{version}-{target}.tar.gz"
            for index, target in enumerate(generator.TARGETS, 1)
        )

    def test_all_platforms_update_together(self):
        formula = generator.render("1.2.3", self.sums())
        self.assertEqual(formula.count('      url "'), 4)
        for index, target in enumerate(generator.TARGETS, 1):
            self.assertIn(f"/v1.2.3/deliveryboy-1.2.3-{target}.tar.gz", formula)
            self.assertIn(f'sha256 "{str(index) * 64}"', formula)
        self.assertIn('bin.install "deliver"', formula)
        self.assertIn('shell_output("#{bin}/deliver --version")', formula)

    def test_invalid_sums_leave_existing_formula_intact(self):
        valid = self.sums()
        invalid_inputs = (
            "\n".join(valid.splitlines()[:-1]),
            valid + "\n" + valid.splitlines()[0],
            valid.replace("1" * 64, "broken", 1),
            self.sums("1.2.2"),
        )
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "SHA256SUMS"
            output = Path(directory) / "deliveryboy.rb"
            output.write_text("keep existing formula\n")
            for sums in invalid_inputs:
                with self.subTest(sums=sums):
                    source.write_text(sums)
                    argv = ["update-homebrew.py", "v1.2.3", "--sha256sums", str(source),
                            "--output", str(output)]
                    with patch("sys.argv", argv), self.assertRaises(SystemExit) as error:
                        generator.main()
                    self.assertEqual(error.exception.code, 2)
                    self.assertEqual(output.read_text(), "keep existing formula\n")

    def test_binary_checksum_marker_and_unrelated_assets(self):
        sums = self.sums().replace("  deliveryboy", " *deliveryboy")
        sums += "\n" + "a" * 64 + "  deliveryboy-1.2.3-windows.zip"
        self.assertEqual(generator.render("1.2.3", sums), generator.render("1.2.3", self.sums()))

    def test_unsafe_or_prerelease_versions(self):
        for version in ("../1.2.3", "1.2.3-rc.1", "1.2", '1.2.3"'):
            with self.subTest(version=version), self.assertRaises(ValueError):
                generator.render(version, self.sums())


if __name__ == "__main__":
    unittest.main()
