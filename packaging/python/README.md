# Python

Peachey Studio releases include CPython 3.12.13 (python-build-standalone build
20260414), PyTheory 0.57.13 and the packages pinned in `requirements.lock`.
They run locally without installing Python, downloading packages or changing
system settings. uv is used only when building a release.

The Python distribution keeps its bundled standard library, native libraries
and license notices. Additional standalone distribution notices from the
20260414 source tag are in `licenses/`; some apply only to specific platforms.
Installed wheels retain their `.dist-info` metadata and
license files, including notices for PyTheory (MIT), NumPy, SciPy, CFFI,
sounddevice and Rich. Optional PyTheory extras aren't included.

Theory, note generation and MIDI/file operations use the bundled packages.
Peel plays the resulting notes through its own instruments. PyTheory's
separate live-audio functions may need platform audio libraries, such as
PortAudio on Linux; they aren't used by Peel's factory actions.

## Build

Install uv 0.11.7 on the build machine, then run on the target OS and CPU:

```sh
uv run --no-project --python 3.12.13 scripts/bundle-python.py target/debug/python
```

Use `--replace` to rebuild an existing Peel Python bundle. The script downloads
standalone CPython into a temporary directory, installs hash-checked wheels,
then moves and tests the runtime with Python removed from PATH. Source
builds and host virtual environments aren't used. Release packaging calls
this script for every platform.

To update packages, edit `requirements.in` and regenerate the lock with:

```sh
uv pip compile packaging/python/requirements.in --python-version 3.12 --universal --generate-hashes --only-binary :all: --output-file packaging/python/requirements.lock
```

Runtime paths are relative to the executable: `Contents/Resources/python` in
the macOS app, `python` beside the Windows executables, and `lib/peel/python`
under the Linux package root. Development builds also look for a `python`
folder beside the executable. The package must keep that folder when moved.

A selected custom interpreter or `PEEL_PYTHON` overrides the bundled runtime.
Use a separate environment for extra packages; changing the shipped files
would invalidate a signed app. Opening a project never installs or runs code.

## macOS signing

The packaging script signs native libraries and Python extension modules before
signing the app. Developer ID builds use hardened runtime. Local ad-hoc builds
have no Team ID and omit hardened runtime so Python can load its bundled
libraries. The signed runtime is tested again before the disk image is built.
