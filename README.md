# ModularClipboard# ModularClipboard
Call/Hotkey →
│ Interface Process (--ui) │
│ Window · Vulkan Rendering · Read List │
└────────────────────────────┘

Reason for Splitting into Two Processes: After the GPU memory of the window is destroyed, it enters the driver pool and is not returned to the system. When the window is closed, the entire block is recycled. The actual measurement shows that the resident memory is reduced from 105MB to 15MB. Only one `Sync{known}` is sent per frame of the interface, and the version does not change back to `{"Unchanged"}` (several tens of bytes), and the full list is not transmitted.

```
modular-clipboard (bin)
├─ modular-clipboard-ui → modular-clipboard-app → {capture, platform, gfx} → store → core
└─ modular-clipboard-mcp → core  # MCP Service: JSON-RPC + UI Automation (stdio / HTTP)
```
See [ARCHITECTURE.md](ARCHITECTURE.md) for details; see [BUILD.md](BUILD.md) for construction and icon resources.

## Actual Measurement Indicators
| Indicator | Value |
|---|---|
| Resident Process Memory (After Interface Closure) | ~15 MB |
| Interface Process Memory (Including Chinese Font 35MB) | ~105 MB |
| Idle CPU | < 1% |
| List with Ten Thousand Entries | Virtualized, No Frame Drop during Scrolling |
Comparison Reference: Bare egui (Only Draw a Label, No Font, No Database) Occupies 128 MB -
The Baseline Cost of the Graphics Stack is Much Higher than the Business Code. This is Also the Reason for Choosing ash Directly Driving Vulkan
(wgpu Initializes Both DX12 and Vulkan Paths at the Same Time. On a Dual Graphics Card Machine, the Memory Increases Several Times).

## Testing
```bash
cargo test --workspace  # 1038 Items (14 Test Binaries)
cargo clippy --workspace --all-targets  # Zero Warnings
```
> The Real Machine Probe (`*_probe.rs` and `full_app.rs` under `crates/*/examples/`) was removed during the repository slimming on 2026-10-23 - They are only used for verifying "Problems that can only be Exposed by Real Shell / Real Message Loop" during the Development Period and do not Belong to the Delivery Content. The Source Code Can Be Found in the `old` Branch.

## Document Map
| What Do You Want to Know | Read This |
|---|---|
| What Is the Project and How to Run It | This File |
| Architecture Layering, Process Model, **Module Map** | [ARCHITECTURE.md](ARCHITECTURE.md) |
| Construction Environment, MSVC Injection, Icon Resources | [BUILD.md](BUILD.md) |
| Version-by-Version Changes | [CHANGELOG.md](CHANGELOG.md) |
| Three-Dimensional Dimension of DPI (Logical Point / Physical Pixel / Normalization) | [docs/dpi-conventions.md](docs/dpi-conventions.md) |
| Findings and Disposal in Each Round of Code Review | [docs/reviews.md](docs/reviews.md) |
| Development History Archive (Parallel Development Blackboard, UI Reconstruction Design Draft) | [docs/history.md](docs/history.md) |
| Memory Analysis / Rendering Pipeline / Compilation Stuck Check | [docs/内存分析-2026-10-12.md](docs/内存分析-2026-10-12.md) etc. |
| Third-Party Licenses | [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) |

## Repository Layout
`main` Only Keeps Four Types of Contents: **Construction Environment** (`Cargo.*`, `scripts/`, `.github/`), **Documents** (4 md at the Root Level + `docs/`), **Source Code** (`crates/`), **Agreement** (`LICENSE` etc.). Intermediate Products Such as Design Prototypes, Screenshots, and Real Machine Probe Scripts Have Been Cleared. The Complete Development History (Including Cleared Files and All Commit Records) Is in the **`old`** Branch.

## Roadmap
- Cross-Platform Verification (Currently Only Windows)
- Load Encryption
- Action Rule Editing Graphical Interface

## Links
- Project Home Page: <https://github.com/MCheng404/ModularClipboard>
- Author Home Page: <https://github.com/MCheng404>
- Agreement: [GNU GPL v3.0 or Later](LICENSE) (`GPL-3.0-or-later`)
