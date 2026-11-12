#!/usr/bin/env python3
"""Check that a running ee2-hyprland uses only bundled GPU libraries."""

import os
import re
import sys


GPU_LIBRARY = re.compile(
    r"^(?:lib(?:EGL|GL(?:X|dispatch|ES(?:v[0-9]+)?|U)?|OpenGL)(?:\.so(?:\..*)?|_[^/]*\.so(?:\..*)?)"
    r"|lib(?:gallium|LLVM|drm|gbm|glapi|vulkan|nvidia)[^/]*"
    r"|[^/]*_dri\.so(?:\.[^/]*)?)$",
    re.IGNORECASE,
)
WEBKIT_PROCESS = "webkitwebprocess"
PROC_ROOT = "/proc"


def read_process(pid):
    """Return (ppid, identification strings), or None if the process vanished."""
    base = os.path.join(PROC_ROOT, str(pid))
    try:
        with open(f"{base}/status", encoding="utf-8") as status_file:
            status = status_file.read()
        with open(f"{base}/comm", encoding="utf-8") as comm_file:
            comm = comm_file.read().strip()
        with open(f"{base}/cmdline", "rb") as cmdline_file:
            raw_cmdline = cmdline_file.read()
            cmdline = raw_cmdline.replace(b"\0", b" ").decode(errors="replace")
            argv0 = raw_cmdline.split(b"\0", 1)[0].decode(errors="replace")
        exe = os.readlink(f"{base}/exe")
    except (FileNotFoundError, ProcessLookupError, PermissionError, OSError):
        return None
    match = re.search(r"^PPid:\s+(\d+)", status, re.MULTILINE)
    if not match:
        return None
    return int(match.group(1)), comm, cmdline, exe, argv0


def process_tree(root_pid):
    """Find root and descendants by walking /proc status PPid fields."""
    records = {}
    try:
        pids = [int(name) for name in os.listdir(PROC_ROOT) if name.isdigit()]
    except OSError as error:
        raise RuntimeError(f"cannot scan /proc: {error}") from error
    for pid in pids:
        record = read_process(pid)
        if record is not None:
            records[pid] = record
    if root_pid not in records:
        raise RuntimeError(f"main process {root_pid} does not exist or disappeared")
    descendants = {root_pid}
    changed = True
    while changed:
        changed = False
        for pid, record in records.items():
            if pid not in descendants and record[0] in descendants:
                descendants.add(pid)
                changed = True
    return [(pid, records[pid]) for pid in sorted(descendants)]


def is_webkit(record):
    comm, _cmdline, exe, argv0 = record[1:]
    if comm.lower() in ("webkitwebprocess", "webkitwebproces"):
        return True
    return os.path.basename(exe).lower() == WEBKIT_PROCESS or os.path.basename(argv0).lower() == WEBKIT_PROCESS


def mapped_gpu_libraries(pid, appdir):
    """Return GPU-library paths mapped outside appdir and all matching names."""
    failures = []
    found = []
    try:
        with open(os.path.join(PROC_ROOT, str(pid), "maps"), encoding="utf-8", errors="replace") as maps_file:
            lines = maps_file.readlines()
    except (FileNotFoundError, ProcessLookupError, PermissionError, OSError):
        return None, None
    for line in lines:
        fields = line.rstrip("\n").split(None, 5)
        if len(fields) < 6 or not fields[5].startswith("/"):
            continue
        path = fields[5].removesuffix(" (deleted)")
        basename = os.path.basename(path)
        if not GPU_LIBRARY.match(basename):
            continue
        found.append(basename)
        resolved = os.path.realpath(path)
        try:
            inside = os.path.commonpath((appdir, resolved)) == appdir
        except ValueError:
            inside = False
        if not inside:
            failures.append(path)
    return found, failures


def main(argv):
    if len(argv) != 3 or not argv[1].isdigit():
        print(f"usage: {argv[0]} PID APPDIR", file=sys.stderr)
        return 2
    pid = int(argv[1])
    appdir = os.path.realpath(argv[2])
    if not os.path.isdir(appdir):
        print(f"error: AppDir does not exist or is not a directory: {argv[2]}", file=sys.stderr)
        return 2
    try:
        processes = process_tree(pid)
    except RuntimeError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    webkit = []
    failed = False
    checked = [(proc_pid, record) for proc_pid, record in processes
               if proc_pid == pid or is_webkit(record)]
    webkit = [proc_pid for proc_pid, record in checked if is_webkit(record)]
    inspected_webkit = 0
    bundled_mesa_egl = False
    for proc_pid, record in checked:
        libraries, outside = mapped_gpu_libraries(proc_pid, appdir)
        if libraries is None:
            print(f"FAIL: cannot read maps for required process {proc_pid} (process disappeared or access denied)")
            failed = True
            continue
        if outside:
            failed = True
            for path in sorted(set(outside)):
                print(f"FAIL: pid {proc_pid} maps host GPU library: {path}")
        else:
            print(f"OK: pid {proc_pid} GPU library mappings are inside AppDir ({len(libraries)} found)")
        if any("libEGL_mesa" in name for name in libraries):
            bundled_mesa_egl = True

        if proc_pid in webkit:
            inspected_webkit += 1

    if not webkit:
        print("FAIL: no WebKitWebProcess found among main process descendants")
        failed = True
    elif inspected_webkit == 0:
        print("FAIL: no WebKitWebProcess maps could be inspected")
        failed = True
    if not bundled_mesa_egl:
        print("FAIL: no checked process maps bundled libEGL_mesa")
        failed = True
    if failed:
        return 1
    print("runtime check: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
