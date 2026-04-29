# deucalion-bridge

A small Windows executable (designed to run under **Wine**) that injects [deucalion](https://github.com/ff14wed/deucalion) into a running FFXIV process and forwards its named pipe over TCP — so that a **native Linux** [FFXIV Teamcraft](https://github.com/birkholz/ffxiv-teamcraft) can receive packet data without any Wine-side network stack involvement.

## How it works

```
FFXIV (Wine) ──named pipe──► deucalion-bridge.exe (Wine) ──TCP 127.0.0.1:31594──► Teamcraft (native Linux)
```

1. Waits for `ffxiv_dx11.exe` to appear in the Wine process list
2. Injects `deucalion.dll` via `LoadLibraryW` in a remote thread
3. Polls for the named pipe `\\.\pipe\deucalion-{pid}` that deucalion creates on load
4. Binds a TCP listener on `127.0.0.1:<port>` (default `31594`)
5. Forwards bytes bidirectionally between the pipe and the TCP socket until either side closes

`deucalion.dll` itself is **not bundled** — it is downloaded at runtime by Teamcraft from the [deucalion releases](https://github.com/ff14wed/deucalion/releases) and cached in the Wine prefix. Deucalion is a separate project by ff14wed, licensed under GPL-3.0.

## Usage

```
deucalion-bridge.exe --dll-path <Windows path to deucalion.dll> [--port <port>]
```

Example (run from Linux via Wine):

```bash
WINEPREFIX=~/.xlcore/wineprefix \
  wine deucalion-bridge.exe \
    --dll-path 'C:\deucalion\deucalion.dll' \
    --port 31594
```

In normal use, Teamcraft spawns and manages this process automatically.

## Building

### Prerequisites

- Rust (stable)
- `x86_64-pc-windows-gnu` target: `rustup target add x86_64-pc-windows-gnu`
- MinGW-w64: `sudo apt install gcc-mingw-w64-x86-64` (Debian/Ubuntu/Arch equivalent)

### Build locally

```bash
# From the deucalion-bridge directory — copies the exe to ../ffxiv-teamcraft/tools/build/
./build.sh
```

Or build manually:

```bash
cargo build --release --target x86_64-pc-windows-gnu
# Output: target/x86_64-pc-windows-gnu/release/deucalion-bridge.exe
```

### CI releases

Pushing a `v*` tag triggers the GitHub Actions workflow, which cross-compiles and publishes `deucalion-bridge.exe` as a GitHub Release asset. Teamcraft's `yarn download:bridge` script fetches the latest release automatically as part of the AppImage build.

## License

MIT
