#!/usr/bin/env python3
"""Focused tests for check-runtime.py using a synthetic /proc tree."""

import contextlib
import importlib.util
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


CHECKER = Path(__file__).with_name("check-runtime.py")
SPEC = importlib.util.spec_from_file_location("check_runtime", CHECKER)
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)


class RuntimeCheckTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.proc = self.root / "proc"
        self.appdir = self.root / "bundle"
        self.proc.mkdir()
        self.appdir.mkdir()
        self.addCleanup(self.temp.cleanup)
        self.addCleanup(patch.stopall)
        patcher = patch.object(checker, "PROC_ROOT", str(self.proc))
        patcher.start()

    def add_process(self, pid, ppid, comm, maps):
        directory = self.proc / str(pid)
        directory.mkdir()
        (directory / "status").write_text(f"Name:\t{comm}\nPPid:\t{ppid}\n")
        (directory / "comm").write_text(comm + "\n")
        command = "WebKitWebProcess" if comm.startswith("WebKitWebProce") else comm
        (directory / "cmdline").write_bytes(command.encode() + b"\0")
        (directory / "exe").symlink_to(f"/opt/{command}")
        (directory / "maps").write_text("".join(
            f"7f000000-7f001000 r-xp 00000000 08:01 123 {path}\n" for path in maps
        ))

    def run_checker(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = checker.main([str(CHECKER), "101", str(self.appdir)])
        return result, output.getvalue()

    def base_processes(self, main_maps=None, web_maps=None):
        main_maps = main_maps or [str(self.appdir / "lib/libgbm.so.1")]
        web_maps = web_maps or [str(self.appdir / "lib/libEGL_mesa.so.0")]
        self.add_process(101, 1, "ee2-hyprland", main_maps)
        self.add_process(102, 101, "WebKitWebProce", web_maps)

    def test_bundled_gpu_libraries_pass(self):
        self.base_processes()
        result, output = self.run_checker()
        self.assertEqual(result, 0, output)
        self.assertIn("runtime check: ok", output)

    def test_mesa_may_be_loaded_by_main_instead_of_webkit(self):
        self.base_processes(
            main_maps=[str(self.appdir / "lib/libEGL_mesa.so.0")],
            web_maps=[str(self.appdir / "lib/libEGL.so.1")],
        )
        result, output = self.run_checker()
        self.assertEqual(result, 0, output)

    def test_bwrap_argument_does_not_count_as_webkit(self):
        self.add_process(101, 1, "ee2-hyprland", [str(self.appdir / "lib/libEGL_mesa.so.0")])
        self.add_process(102, 101, "bwrap", [])
        (self.proc / "102/cmdline").write_bytes(b"bwrap\0WebKitWebProcess\0")
        result, output = self.run_checker()
        self.assertEqual(result, 1)
        self.assertIn("no WebKitWebProcess", output)

    def test_missing_bundled_mesa_everywhere_fails(self):
        self.base_processes(
            main_maps=[str(self.appdir / "lib/libgbm.so.1")],
            web_maps=[str(self.appdir / "lib/libEGL.so.1")],
        )
        result, output = self.run_checker()
        self.assertEqual(result, 1)
        self.assertIn("no checked process maps bundled libEGL_mesa", output)

    def test_host_library_fails_even_when_path_shares_appdir_prefix(self):
        sibling = self.root / "bundle-other" / "lib/libEGL_mesa.so.0"
        sibling.parent.mkdir(parents=True)
        sibling.touch()
        self.base_processes(web_maps=[str(sibling)])
        result, output = self.run_checker()
        self.assertEqual(result, 1)
        self.assertIn("maps host GPU library", output)

    def test_gpu_names_exclude_glib_and_include_gles(self):
        for name in ["libGLESv2.so.2", "libEGL_mesa.so.0", "libglapi.so.0"]:
            self.assertIsNotNone(checker.GPU_LIBRARY.match(name), name)
        for name in ["libglib-2.0.so.0", "libglycin-2.so.0"]:
            self.assertIsNone(checker.GPU_LIBRARY.match(name), name)

    def test_missing_webkit_fails(self):
        self.add_process(101, 1, "ee2-hyprland", [])
        result, output = self.run_checker()
        self.assertEqual(result, 1)
        self.assertIn("no WebKitWebProcess", output)

    def test_unreadable_main_maps_fail(self):
        self.base_processes()
        (self.proc / "101/maps").unlink()
        result, output = self.run_checker()
        self.assertEqual(result, 1)
        self.assertIn("cannot read maps for required process 101", output)


if __name__ == "__main__":
    unittest.main()
