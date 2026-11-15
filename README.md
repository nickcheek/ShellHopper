# ShellHopper

<p align="center">
  <img src="assets/ShellHopperIcon.png" width="180" alt="ShellHopper icon">
</p>

ShellHopper is a native macOS terminal for organizing SSH hosts and working across several servers without turning the terminal into a project-management system.

It uses your real shell and normal OpenSSH setup. There is no account, telemetry, cloud sync, subscription, or AI running in the app. It just helps you hop between shells. The frog jokes mostly stop there.

## Download

Download the latest signed and notarized DMG from [GitHub Releases](https://github.com/nickcheek/ShellHopper/releases). Open it, then drag ShellHopper into Applications.

## Features

- Import hosts from `~/.ssh/config`, including aliases, keys, ports, agents, and `ProxyJump` settings.
- Organize hosts into collapsible, reorderable groups with optional separators.
- Open local or SSH sessions in tabs and recursively split panes right or down.
- Select several hosts and run one command or multiline script in a managed tab for each server.
- Save shared, single-line commands and paste them from a terminal's right-click menu. ShellHopper never presses Enter for you.
- Open saved hosts from the macOS menu bar.
- Drop local files onto a connected SSH pane to upload them to that host's home directory.
- Search terminal output and the commands recorded for the current session.
- Restore tab order, splits, host organization, and other safe interface state after restarting.
- Use normal terminal behavior: truecolor, mouse reporting, alternate screens, bracketed paste, scrollback, selection, `vim`, `less`, and friends.

## Requirements

- macOS
- A current stable Rust toolchain when building from source
- OpenSSH, included with macOS

The optional local SSH fixture also requires Docker.

## Build from source

```sh
git clone https://github.com/nickcheek/ShellHopper.git
cd ShellHopper
cargo build --release
./target/release/shellhopper
```

## Package a release

The packaging script builds a universal app for Apple silicon and Intel Macs, signs it with hardened runtime, creates a DMG, submits it to Apple for notarization, staples the approval ticket, and verifies the finished download.

```sh
DEVELOPER_ID_APPLICATION="Developer ID Application: Your Name (TEAMID)" \
NOTARY_PROFILE="ShellHopper-notary" \
./scripts/package-macos.sh
```

The finished disk image is written to `dist/`. Signing requires a Developer ID Application certificate, and `NOTARY_PROFILE` must name credentials stored with `xcrun notarytool store-credentials`.

## Using ShellHopper

On first launch, ShellHopper imports usable entries from `~/.ssh/config`. You can also add hosts manually, reload the SSH config, reorganize hosts, or connect a pane to another saved host from its right-click menu.

To work across several servers, select the hosts in the sidebar, enter a command or script, and choose **Run on selected hosts**. ShellHopper opens or reuses one managed tab per host so each server's output stays separate.

### Saved commands

Right-click a terminal and choose **Saved commands** to paste a saved command into that pane. Review or edit it, then press Enter yourself.

Choose **Manage saved commands…** from the same menu to add, rename, reorder, or remove commands. Saved commands are shared across all hosts and are limited to one line so selecting one cannot hide an embedded Enter.

### Keyboard shortcuts

| Shortcut | Action |
| --- | --- |
| `⌘T` | Open a local terminal |
| `⌘W` | Close the active pane or tab |
| `⌘⇧T` | Reopen the last closed tab |
| `⌘1`–`⌘8` | Switch to a numbered tab |
| `⌘9` | Switch to the last tab |
| `⌘⇧[` / `⌘⇧]` | Switch tabs |
| `⌘F` | Search terminal output |
| `⌘L` | Search, copy, or paste commands from the current session |
| `⌘K` | Clear the active terminal and scrollback |
| `⌘D` | Split the active pane right |
| `⌘⇧D` | Split the active pane down |
| `⌘↓` | Return to live output |
| `⇧Page Up` / `⇧Page Down` | Move through scrollback |
| `⌘` + click a URL or local path | Open it with the default macOS app |
| Shift + drag | Select text when a terminal program owns the mouse |

## Configuration and privacy

ShellHopper stores its configuration here:

```text
~/Library/Application Support/ShellHopper/config.toml
```

An existing TermiSlop configuration is copied into this location the first time ShellHopper starts. The old file is left untouched.

The configuration contains host organization, saved commands, and interface state. It does not store passwords, terminal output, environment variables, SSH credentials, pending commands, or scrollback. Authentication remains with your SSH config, keys, agent, and interactive prompts.

Saved commands are plain text. Treat the list like a shell script: commands belong there; passwords and tokens do not.

Restored SSH panes start disconnected and reconnect only when you ask them to or send a multi-host command to that server.

## Local SSH fixture

The opt-in Docker/OpenSSH fixture under [`tests/fixtures/ssh`](tests/fixtures/ssh/README.md) binds to a random loopback port and creates disposable keys and configuration under `/tmp`. It does not modify your real SSH config or `known_hosts` file.

```sh
./tests/fixtures/ssh/start.sh
./tests/fixtures/ssh/stop.sh
```

## Contributing

Bug reports should include what you did, what you expected, what happened, and enough detail to reproduce it. Small pull requests that solve one real problem are much easier to review than ambitious plans to reinvent the terminal.

## License

ShellHopper is available under the [MIT License](LICENSE).
