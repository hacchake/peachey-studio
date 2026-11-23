# Asset licenses

Every bundled asset, its source and its license. Add a row for each new
asset in the same commit that adds it.

| Asset | Source | Version | License | Use |
| --- | --- | --- | --- | --- |
| `fonts/barlow/BarlowSemiCondensed-*.ttf` | [google/fonts `ofl/barlowsemicondensed`](https://github.com/google/fonts/tree/main/ofl/barlowsemicondensed), Jeremy Tribby | main, fetched 2026-11-09 | SIL OFL 1.1 (`fonts/barlow/OFL.txt`) | Interface text |
| `icons/peachey-*.png`, `icons/peachey.ico` | Peach artwork made for Peachey Studio by hacchake | — | GPL-3.0-or-later (project artwork) | App and window icon |
| `fonts/phosphor/Phosphor.ttf`, `Phosphor-Fill.ttf` | [`@phosphor-icons/web`](https://github.com/phosphor-icons/web) | 2.1.2 | MIT (`fonts/phosphor/LICENSE`) | Interface icons |
| `drums/big-rusty/*.wav` (96 recordings) | [Karoryfer Samples, Big Rusty Drums](https://shop.karoryfer.com/pages/free-big-rusty-drums), [source](https://github.com/sfzinstruments/karoryfer.big-rusty-drums) | `f07ce00df34a46b6b08375be56fe116cf15782bc`, fetched 2026-11-10; source files and processing in `drums/big-rusty/manifest.json` | CC0-1.0 (`drums/big-rusty/LICENSE.txt`) | Embedded recorded acoustic kit |
| `strings/recorded/guitar-*.wav` (160 recordings) | [Karoryfer Samples, Black And Green Guitars](https://shop.karoryfer.com/pages/free-black-and-green-guitars) | `b3b3249d37dc977a1a297bd2dc053e6d9b6b805c`, fetched 2026-11-10; details in `strings/recorded/manifest.json` | CC0-1.0 (`strings/recorded/LICENSE-guitar.txt`) | Embedded sustained, staccato and release guitar recordings |
| `strings/recorded/bass-*.wav` (84 recordings) | [Karoryfer Samples, Black And Blue Basses](https://shop.karoryfer.com/pages/free-black-and-blue-basses) | `6e7d674cdb41be7a54dbccb15472401ad01099b9`, fetched 2026-11-10; details in `strings/recorded/manifest.json` | CC0-1.0 (`strings/recorded/LICENSE-bass.txt`) | Embedded picked bass recordings |
| `strings/palm-muted/palm-*.wav` (133 recordings) | [Guitar-TECHS v1](https://zenodo.org/records/14963133), Player 2 | `P2_techniques.zip`, fetched 2026-11-10; member CRCs, hashes and processing in `strings/palm-muted/manifest.json` | CC-BY-4.0 (`strings/palm-muted/LICENSE.txt`, attribution in `NOTICE.txt`) | Embedded palm-muted guitar recordings; retain credit in published audio |
| `scripts/*.js`, `scripts/*.lua`, `scripts/*.py` | Written for Peel Studio | API 1 | GPL-3.0-or-later (project code) | Factory scripting actions |

Release packages also include a private CPython runtime and Python packages:

| Component | Version | Notices |
| --- | --- | --- |
| CPython / python-build-standalone | 3.12.13 / 20260414 | PSF and component notices in the runtime's LICENSE files and `licenses/`; source notices are in `packaging/python/licenses/` |
| PyTheory | 0.57.13 | MIT; retained in its `.dist-info` metadata |
| NumPy, SciPy, sounddevice, CFFI, Rich and their dependencies | Pinned in `packaging/python/requirements.lock` | Each wheel retains its own license files and notices |

See [packaging/python/README.md](../packaging/python/README.md) for runtime
sources, dependency pins and build instructions.

Acoustic Kit, Recorded Guitar, Palm-Muted Guitar and Recorded Bass use bundled recordings.
The other native instruments synthesize their factory sounds at runtime.
