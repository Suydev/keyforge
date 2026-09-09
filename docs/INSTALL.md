# Installation

The gateway is a single static-ish binary plus a launcher script and a config
directory. There is no daemon manager, no container, and no database.

## Supported platforms

| Platform | Build | Run | Wi-Fi link sampling |
| --- | --- | --- | --- |
| Android / Termux (aarch64) | yes | yes | **yes** — full, via `rish` + `dumpsys` |
| Linux (x86_64, aarch64) | yes | yes | no — inert, budgets unaffected |
| macOS (Intel, Apple Silicon) | yes | yes | no — inert |
| Windows 10/11 (x86_64) | yes | yes | no — inert |

Link sampling reads Android system services, so it only functions there. On every
other platform `link.rs` short-circuits: nothing is spawned, the quality bucket
stays `Unknown`, and the timeout multiplier stays exactly `1.0`. The gateway
behaves as though the module were absent — no warnings, no penalty.

Termux is the primary target and the only platform where the whole stack has been
run continuously for days. The others build and pass the test suite.

## Prerequisites

- **Rust 1.75+** with `cargo`. The installer offers to fetch it via `rustup` if
  missing.
- **A C linker.** `clang` on Termux, `gcc`/`cc` on Linux, Xcode command line
  tools on macOS, MSVC build tools on Windows. `ring` (the crypto backend) needs
  one.
- **At least one API key** for a [new-api](https://github.com/QuantumNous/new-api)
  compatible provider.
- ~400MB of disk for the build, ~2MB for the resulting binary.
- `python3` — optional, only for the richer `tabi status` output.

## Quick install

```sh
curl -fsSL https://raw.githubusercontent.com/Suydev/tabi-gateway/main/install.sh | sh
```

If you would rather read it first — which is the correct instinct for anything
piped into a shell:

```sh
curl -fsSLO https://raw.githubusercontent.com/Suydev/tabi-gateway/main/install.sh
less install.sh
sh install.sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/Suydev/tabi-gateway/main/install.ps1 | iex
```

### Installer flags

```
--prefix DIR      install root                  (default ~/.local)
--src DIR         where to keep the source      (default ~/tabi-gateway)
--port N          listen port                   (default 8787)
--no-build        install scripts only, skip cargo
--no-rust         fail instead of offering rustup
--force           overwrite an existing install
--uninstall       remove binary, launcher, and config (keys are kept)
-h, --help
```

### What it does, in order

1. Detects OS and architecture; refuses combinations it cannot build.
2. Checks for `cargo` and a linker. Offers `rustup` if Rust is missing; never
   installs it silently.
3. Copies or clones the source to `--src`.
4. `cargo build --release`.
5. Installs the binary to `<prefix>/bin/tabi-gateway`.
6. Generates the `tabi` launcher **for your platform** — the shebang and paths
   are written at install time rather than shipped hardcoded.
7. Creates `~/.config/tabi/` and writes a starter `providers.json` if absent.
8. Adds `<prefix>/bin` to `PATH` in your shell profile if it is missing, and says
   so rather than doing it quietly.
9. Prints where to put keys and how to start.

It does **not** create key files, write credentials, enable a system service, or
open a firewall port.

## Manual install

```sh
git clone https://github.com/Suydev/tabi-gateway
cd tabi-gateway
cargo build --release

mkdir -p ~/.local/bin ~/.config/tabi
cp target/release/tabi-gateway ~/.local/bin/

# Keys: one per line, filename must match providers.json
mkdir -p ~/keys
printf 'sk-your-key-here\n' > ~/keys/provider-a-keys.txt

~/.local/bin/tabi-gateway            # foreground, Ctrl-C to stop
```

Run it in the foreground the first time. Startup prints the key count and total
known balance per provider, which is the fastest way to confirm your key files
are being read.

## Configuration

`~/.config/tabi/providers.json` is written on first run and is editable while the
gateway runs. Minimal shape:

```json
{
  "version": 1,
  "providers": [
    {
      "id": "provider-a",
      "label": "Provider A",
      "hosts": [{ "host": "api.example.com", "enabled": true, "note": "primary" }],
      "keys_file": "keys/provider-a-keys.txt",
      "hold": 0.80,
      "initial_guess": 120.0,
      "enabled": true
    }
  ]
}
```

`keys_file` is relative to `$HOME`. `hold` is the provider's per-request
pre-deduction; if you do not know it, guess low — the gateway corrects it from the
first quota refusal it sees.

Full reference: [CONFIGURATION.md](CONFIGURATION.md).

## Optional: egress proxy pool

Upstream rate limits are commonly per-IP as well as per-account. Without
rotation, many accounts can be limited together because they all egress from one
address.

Create `~/proxies.txt`, one endpoint per line, in any of:

```
host:port:user:pass
http://user:pass@host:port
host:port
```

Missing or empty means all traffic goes direct, which works but forfeits
rotation.

## Optional: Wi-Fi link sampling (Android)

Full sampling needs [Shizuku](https://shizuku.rikka.app/) and its `rish` shell.

```sh
# From the Shizuku app, export rish + rish_shizuku.dex, then:
mkdir -p ~/.local/rish
cp /path/to/rish ~/.local/rish/
cp /path/to/rish_shizuku.dex ~/.local/rish/
chmod 400 ~/.local/rish/rish_shizuku.dex   # Android 14+ refuses a writable dex
chmod 700 ~/.local/rish/rish
sed -i 's/RISH_APPLICATION_ID="PKG"/RISH_APPLICATION_ID="com.termux"/' ~/.local/rish/rish
~/.local/rish/rish -c 'id -un'             # expect: shell
```

Three details that will otherwise waste your afternoon:

- Android 14+ will not load a writable dex — hence `chmod 400`.
- `rish` finds its dex via `dirname "$0"`, so a PATH **symlink breaks it**. Use a
  wrapper that `exec`s the real path.
- The bundled script ships `RISH_APPLICATION_ID="PKG"` as a literal placeholder.

Without Shizuku, the gateway falls back to `termux-wifi-connectioninfo` from
Termux:API, which gives RSSI and link speed but not Android's own link score or
retry counters.

## Verify

```sh
curl -s http://127.0.0.1:8787/api/health          # {"ok":true,...}
tabi status                                        # per-provider summary
```

Then open `http://127.0.0.1:8787/` for the dashboard.

## Upgrading

```sh
cd ~/tabi-gateway && git pull && cargo build --release && tabi restart
```

State in `~/.config/tabi/` is forward-compatible; unknown fields are ignored and
missing ones take their defaults.

**Do not restart from a session routed through the gateway** — you will cut your
own connection. Check with `tabi status` first.

## Uninstall

```sh
./install.sh --uninstall
```

Removes the binary, the launcher, and `~/.config/tabi/`. **Key files are left
alone** — they are yours, they may be the only copy, and a script should not
delete them.

## Troubleshooting the install

**`linker cc not found`** — install a compiler: `pkg install clang` (Termux),
`apt install build-essential` (Debian), `xcode-select --install` (macOS).

**Build killed partway** — out of memory. `cargo build --release -j1`. On a
tablet, plug in first; a release build pins every core for minutes.

**`another tabi-gateway is already using ...`** — a PID lockfile is doing its job.
`tabi stop`, or point `TABI_STATE` at a different file for a second instance.

**`port 8787 is held by something that is not the gateway`** — the launcher
detected a squatter. `tabi restart` replaces it.

**Zero keys at startup** — `keys_file` paths are relative to `$HOME`. Check the
path and that the file has one key per line.
