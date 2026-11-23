#!/usr/bin/env python3
"""Build a relocatable Python + PyTheory distribution. Requires uv at build time."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parent.parent
VERSION = "3.12.13"
BUILD = "20260414"
REQUIREMENTS = ROOT / "packaging/python/requirements.lock"
MARKER = "peel-python.json"
SMOKE = """
import json, sys, importlib.metadata
from pathlib import Path
import numpy, scipy
from scipy import signal
from pytheory import TonedScale
assert [tone.midi for tone in TonedScale(tonic='C4')['major'].triad().tones] == [60, 64, 67]
assert signal.convolve([1, 2], [1]).tolist() == [1, 2]
assert sys.version_info[:3] == (3, 12, 13)
assert importlib.metadata.version('pytheory') == '0.57.13'
assert Path(numpy.__file__).is_relative_to(Path(sys.prefix))
assert Path(scipy.__file__).is_relative_to(Path(sys.prefix))
print('Bundled Python and PyTheory passed relocation checks.')
"""


def python_executable(root):
    return root / ("python.exe" if sys.platform == "win32" else "bin/python3")


def run(args, *, env):
    subprocess.run([str(arg) for arg in args], check=True, env=env, cwd=ROOT)


def bundle(destination, replace=False):
    uv = shutil.which("uv")
    if not uv:
        raise RuntimeError("Install uv to build the Python bundle; end users do not need it.")
    destination = destination.absolute()
    if destination.exists() and not (replace and (destination / MARKER).is_file()):
        raise RuntimeError(f"Refusing to replace {destination}; use --replace only for an existing Peel Python bundle.")
    destination.parent.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env.update(UV_PYTHON_CPYTHON_BUILD=BUILD, UV_PYTHON_INSTALL_BIN="false")
    # Fresh managed CPython, not a venv tied to the build machine's interpreter.
    with tempfile.TemporaryDirectory(prefix=".peel-python-", dir=destination.parent) as temporary:
        temporary = Path(temporary)
        managed = temporary / "managed"
        run([uv, "--no-config", "python", "install", VERSION, "--install-dir", managed,
             "--no-bin", "--no-registry"], env=env)
        distributions = [p for p in managed.glob("cpython-*") if p.name.startswith(f"cpython-{VERSION}-") and p.is_dir() and not p.is_symlink() and not p.is_junction()]
        if len(distributions) != 1:
            raise RuntimeError(f"Expected one standalone CPython distribution, found {distributions}")
        runtime = temporary / "python"
        shutil.copytree(distributions[0], runtime, symlinks=True)
        # Do not retain uv's install location in the Windows discovery registry.
        interpreter = python_executable(runtime)
        # Target the interpreter's actual site-packages (Windows uses Lib/).
        site = subprocess.check_output([str(interpreter), "-I", "-c",
            "import sysconfig; print(sysconfig.get_path('purelib'))"], env=env, text=True).strip()
        site = Path(site)
        if not site.is_relative_to(runtime):
            raise RuntimeError(f"Standalone Python still points outside the bundle: {site}")
        run([uv, "--no-config", "pip", "install", "--python", interpreter,
             "--target", site, "--link-mode", "copy", "--only-binary", ":all:",
             "--require-hashes", "--requirements", REQUIREMENTS], env=env)
        # Entry-point wrappers contain build-machine shebangs and are not used.
        # All supported script imports and third-party metadata/licenses stay intact.
        for scripts in (site / "bin", site / "Scripts", runtime / "Scripts"):
            if scripts.is_dir():
                shutil.rmtree(scripts)
        if sys.platform != "win32":
            for entry in (runtime / "bin").iterdir():
                if entry.name not in {"python", "python3", f"python{VERSION.rsplit('.', 1)[0]}"}:
                    entry.unlink()
        # Hash-based caches survive zip timestamp rounding and avoid compiling
        # the standard library and NumPy during every user action. The Windows
        # build's tcl/ folder is Tcl/Tk data, and one of its sample .py files
        # mixes tabs and spaces, so it is skipped.
        run([interpreter, "-I", "-m", "compileall", "-q", "-j", "2",
             "--invalidation-mode", "unchecked-hash",
             "-x", re.escape(str(runtime / "tcl")), runtime], env=env)
        info = {
            "application": "Peachey Studio", "schema": 1,
            "python": VERSION, "python_build": BUILD, "pytheory": "0.57.13",
            "platform": sys.platform, "architecture": platform.machine(),
            "requirements_sha256": hashlib.sha256(REQUIREMENTS.read_bytes()).hexdigest(),
        }
        (runtime / MARKER).write_text(json.dumps(info, indent=2) + "\n", encoding="utf-8")
        shutil.copytree(ROOT / "packaging/python/licenses", runtime / "licenses")
        shutil.copy2(REQUIREMENTS, runtime / "requirements.lock")
        shutil.copy2(ROOT / "packaging/python/README.md", runtime / "README.md")
        # Move and run with a clean launch environment to catch absolute paths,
        # missing dylibs and accidental imports from the developer's environment.
        relocated = temporary / "Relocated Python"
        runtime.rename(relocated)
        clean = {k: v for k, v in env.items() if not k.startswith(("PYTHON", "UV_", "DYLD_", "LD_"))}
        clean["PATH"] = str(python_executable(relocated).parent)
        run([python_executable(relocated), "-I", "-B", "-c", SMOKE], env=clean)
        if destination.exists():
            # A valid replacement is staged and verified before the old one moves.
            old = temporary / "previous"
            destination.rename(old)
        relocated.rename(destination)
    print(f"Bundled Python {VERSION} and PyTheory 0.57.13 in {destination}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--replace", action="store_true", help="Replace an existing Peel Python bundle")
    args = parser.parse_args()
    try:
        bundle(args.destination, args.replace)
    except (RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
