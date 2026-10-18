# Aura Clipboard

One clipboard for all the computers on your network. Copy on one PC and paste
on any other. It runs on Windows, macOS and Linux, talks to your other PCs
directly with end-to-end encryption, and needs no server or account.

## Install

Download the file for your system from the
[latest release](https://github.com/yahgos/aura-clipboard/releases/latest)
and run it. You don't need to install anything else.

| System | Download | Then |
| --- | --- | --- |
| macOS 11 or later (Apple Silicon and Intel) | `…_universal.dmg` | Open it and drag Aura Clipboard to Applications |
| Windows 10 or 11 | `…_x64-setup.exe` | Run it (no admin rights needed) |
| Linux, any distro | `….AppImage` | Make it executable and run it (see below) |
| Linux, Debian or Ubuntu | `….deb` | `sudo apt install ./Aura*.deb` |

### First launch

The builds aren't code-signed yet, so each OS asks for confirmation the first
time you open Aura.

<details>
<summary><b>macOS</b>: "Apple could not verify Aura Clipboard"</summary>

1. Open Aura Clipboard from Applications. macOS refuses it the first time.
2. Go to **System Settings → Privacy & Security**, scroll down and click
   **Open Anyway** next to Aura Clipboard.
3. When macOS asks whether Aura may find devices on your local network, click
   **Allow**. Aura can't see your other PCs without this permission.

Instead of step 2 you can run
`xattr -dr com.apple.quarantine "/Applications/Aura Clipboard.app"` in Terminal.

Aura runs in the menu bar and has no Dock icon.
</details>

<details>
<summary><b>Windows</b>: "Windows protected your PC"</summary>

1. In the SmartScreen dialog, click **More info**, then **Run anyway**.
2. When Windows Defender Firewall asks, allow access on private networks.

Aura runs in the system tray at the bottom right. Windows may hide the icon
under the **^** arrow.
</details>

<details>
<summary><b>Linux</b>: AppImage and tray icon</summary>

```bash
chmod +x Aura*.AppImage
./Aura*.AppImage
```

If the AppImage complains that FUSE is missing, install `libfuse2` (on Ubuntu
24.04 and later, `libfuse2t64`), or start it without FUSE with
`./Aura*.AppImage --appimage-extract-and-run`.

GNOME hides tray icons unless you install the
[AppIndicator extension](https://extensions.gnome.org/extension/615/appindicator-support/).
KDE, Cinnamon and XFCE show the icon without extra setup.

If you run a firewall such as `ufw`, open the ports listed under
[If the PCs don't find each other](#if-the-pcs-dont-find-each-other).
</details>

## Getting started

1. On the first PC, Aura opens a window. Under **Create a group**, pick a name
   such as "Home" and a passphrase of at least 8 characters.
2. On each other PC, open Aura. The group appears under **Join a group**.
   Select it, type the same passphrase and click **Join**.
3. Copy text or an image on any of the PCs and paste it on another.

To keep Aura running all the time, turn on **Start at login** in the tray menu
on each PC.

The tray menu also lists recent entries; clicking one puts it back on the
clipboard of every PC. **Pause sync** stops this PC from sending or receiving
for a while. **Devices & settings** shows which PCs are online and lets you
remove a PC, change the passphrase or leave the group.

Aura never shares a copy that a password manager has marked as secret.
KeePassXC marks its copies on all three systems.

### If the PCs don't find each other

Discovery only works when the PCs are on the same network, and guest Wi-Fi and
some routers block it. Firewalls need to allow TCP ports 47820 and 47821 and UDP
port 5353 (mDNS).

You can skip discovery: click **Enter an address instead** in the Join form and
type the other PC's IP address, for example `192.168.1.20`. Each PC shows its
own address at the top of Aura's window.

### Removing a lost PC

Open **Devices & settings**, click **Remove** next to the PC, then use **Change
passphrase**. The removed PC stops receiving clipboard entries immediately, and
the new passphrase keeps it from joining again. Your other PCs stay connected.

## Use it from your phone

One of your PCs, the fofoqueiro, also serves your clipboard as a web page, so
your phone can see the history, copy entries and send text to the PCs without
installing anything. The PCs pick the fofoqueiro themselves, and another one
takes over when it shuts down.

On the phone:

1. Open `https://aura.local:47821`, or scan the QR code that **Show QR code**
   displays in Aura's window on any PC. The QR code also logs you in.
2. The browser warns that the connection isn't private, because the
   certificate belongs to your group instead of a public authority. Accept it
   once: on iPhone tap **Show Details**, then **visit this website**; on
   Android tap **Advanced**, then **Proceed**. All your PCs share the
   certificate, so the warning doesn't come back when another PC takes over.
3. If you didn't scan the QR code, log in with the group passphrase. The login
   lasts 30 days from your last visit and ends when the passphrase changes.
4. To get an app-like icon, use **Add to Home Screen** from the browser's
   share or menu button.

If `aura.local` doesn't open (some Android versions and networks don't resolve
`.local` names), use the IP address Aura's window shows under **Phone**. A PC
can opt out of serving the page with the **Allow this PC to serve the phone
page** checkbox, or with `aura run --no-web`. Firewalls need to allow TCP port
47821.

## Command-line version

If you prefer a terminal to a tray icon, each release also includes a
standalone `aura` binary, `aura-cli-…-<platform>.tar.gz` (a `.zip` on Windows).
Unpack it and run it; there's nothing to install.

```bash
aura run --create-group Home      # first PC
aura discover                     # other PCs: list groups nearby
aura run --join-group <group-id>  # …or: aura run --join 192.168.1.20
aura run                          # every later start
```

`aura` asks for the passphrase, or reads it from `AURA_PASSPHRASE`.
`aura run --change-passphrase` changes it for the whole group, and `aura watch`
prints clipboard changes so you can check that your OS is supported. The CLI and
the tray app share their settings, so run only one of them on a PC.

## Settings

The first run creates `config.toml` in the data directory:

- macOS: `~/Library/Application Support/dev.aura.aura-clipboard`
- Linux: `~/.local/share/aura-clipboard`
- Windows: `%APPDATA%\aura\aura-clipboard\data`

The file holds the device name, the port (47820 by default), static peers for
networks without multicast, size limits, the history length, the secret store
(`keyring` or `file`), the phone page port (47821) and whether this PC may serve
the phone page (`web_enabled`). The keyring is the macOS Keychain, the Windows
Credential Manager, or the Secret Service (GNOME Keyring or KWallet) on Linux.
When no keyring is available, Aura keeps its secrets in a file in the data
directory that only your user can read. Debug builds always use that file, so
macOS doesn't ask for keychain access after every rebuild.

## How it works

Every PC advertises `_aura._tcp.local.` over mDNS, which is how a new PC finds
the groups on the network before asking for the passphrase.

Joining checks the passphrase with SPAKE2, a password-authenticated key
exchange, and its result keys a Noise `XXpsk3` handshake. The passphrase never
crosses the network, so an eavesdropper can't brute-force it offline, and
members rate-limit guesses. After joining, devices recognize each other by
their Noise static keys.

Membership is a 2P-set CRDT that peers exchange on every connection. Removing a
device bans its key for good; the removed PC learns it was removed and leaves
the group.

Changing the passphrase sends a new versioned key to every member. If two PCs
change it at the same time, the newer version wins, and PCs that were offline
receive the key when they reconnect. Existing members stay in the group; only
PCs that join later need the new passphrase. Change it after removing a lost
PC, because that PC knew the old one.

Each copy becomes an entry stamped with a hybrid logical clock, and the newest
entry wins on every PC, with the device id breaking ties. Entries spread
through the full mesh, and duplicates are dropped. A PC that was asleep catches
up when it reconnects.

The phone page is served by one PC at a time. PCs that allow it hold an
election, and the lowest device id among the connected ones wins after holding
that spot for three seconds. The winner claims the `aura.local` name over mDNS
and serves HTTPS with a certificate shared by the whole group. Phone logins are
signed tokens derived from the group key, so they work on whichever PC serves
the page and stop working when the passphrase changes.

Aura syncs text, HTML with its plain-text version, and images, which travel as
PNG. It skips copies that carry a password-manager marker:
`org.nspasteboard.ConcealedType` on macOS,
`ExcludeClipboardContentFromMonitorProcessing` on Windows and
`x-kde-passwordManagerHint` on Linux. Aura also fingerprints what it writes for
other PCs, so their entries don't bounce back as new copies.

| Crate | Role |
| --- | --- |
| [`aura-core`](crates/aura-core) | Pure logic: HLC, entries, LWW history, membership CRDT (property-tested) |
| [`aura-net`](crates/aura-net) | Wire protocol, Noise sessions with chunking, SPAKE2 enrollment, mDNS |
| [`aura-clip`](crates/aura-clip) | OS clipboard (arboard + native change counters), echo suppression |
| [`aura-node`](crates/aura-node) | The engine: actor, storage (redb), secrets (keychain/file), `NodeHandle` API |
| [`aura-web`](crates/aura-web) | The phone page: HTTPS server, sessions, QR login, embedded web app |
| [`aura-cli`](apps/aura-cli) | `aura` binary: headless runner and debugging |
| [`aura-tray`](apps/aura-tray) | Tauri v2 tray app |

## Development

```bash
cargo test            # core, net, clip, node (incl. multi-node integration tests), cli
cargo clippy --workspace --all-targets
cargo build -p aura-tray
```

The integration tests in [`crates/aura-node/tests/sync.rs`](crates/aura-node/tests/sync.rs)
run several nodes in one process with in-memory clipboards. They cover:

- propagation without echo
- concurrent copies converging on one entry
- rejecting a wrong passphrase
- removing a device and letting it rejoin
- catching up after a restart
- 1 MB images
- pausing, and recopying from history
- passphrase changes, including PCs that were offline and two changes at once
- electing the PC that serves the phone page, failover, and opting out

[`crates/aura-web/tests/web.rs`](crates/aura-web/tests/web.rs) drives the phone
page over real HTTPS: login and rate limiting, QR login, history and images,
sending text, live updates, logins surviving failover, and logout after a
passphrase change.

`crates/aura-clip/tests/x11_marker.rs` checks the KDE password marker against a
real X server. It needs Xvfb and `xclip`:
`DISPLAY=:99 cargo test -p aura-clip -- --ignored`.

### Building the installers

```bash
cargo install tauri-cli --version "^2" --locked
cd apps/aura-tray && cargo tauri build      # → target/release/bundle/
```

### Publishing a release

Push a version tag. The [release workflow](.github/workflows/release.yml) builds
the macOS, Windows and Linux installers and the CLI binaries, then attaches them
to a draft release that you check and publish.

```bash
git tag v0.1.0 && git push origin v0.1.0
```

## Status and roadmap

Everything above works. On macOS, the test suite passes, and the `.dmg` was
built, checked to link only system libraries, and launched. On Linux, the test
suite, the X11 password-marker check and the keyring fallback passed in Docker,
where the `.deb` and `.AppImage` were also built and launched. Windows is only
compiled in CI so far. Windows and Linux desktops still need testing on real
machines.

Planned next:

- GNOME on Wayland, which lacks the data-control protocol (Aura uses XWayland
  there for now)
- Signed installers, and a firewall rule added on first run on Windows
- Copying and pasting files
