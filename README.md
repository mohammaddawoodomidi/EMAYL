# EMAYL

A clean, fast desktop email program for **Windows** and **Linux**, written in Rust.
Light and dark look, any number of accounts, one unified inbox.

> **Status: early version (0.2).** Tested with simulated mail servers and a built-in screen test;
> not yet used with real accounts or on a real Windows computer. Please report what you find.

## Features

- **Many accounts** in one unified inbox or one by one, each with its own colour.
- **Easy setup:** type your address; the server settings are found for you (Gmail, Outlook.com,
  Yahoo, iCloud, Fastmail, Zoho, GMX and many others). Your own server works too.
- **Read and organise:** conversations (newest message first), search, stars, archive, delete,
  junk, blocked senders and simple filter rules, select several at once for bulk actions.
- **Write:** reply, forward, attachments, drafts, templates, and *Undo send*. Messages that cannot
  be delivered are explained in plain words, with *Try again* in the Outbox.
- **Your look:** light, dark or "match my computer"; accent colour, size, row density, list width,
  and whether the message opens beside the list or on its own page.
- **Fast:** recent mail is fetched ahead of time, long lists stay smooth, and replies fold away the
  quoted text so the new message stands out.
- **Private by design.**

## Download

Get the latest version from the **Releases** page:

| System | File |
|---|---|
| Windows 10/11 (64-bit) | `emayl-for-windows-x86_64.exe` (installer) or `emayl-windows-x86_64.zip` (unzip and run) |
| Ubuntu, Debian, Mint | `emayl_<version>_amd64.deb` (double-click it) |
| Other Linux (64-bit) | `emayl-linux-x86_64.tar.gz` (unpack, run `./emayl`) |

Each file comes with a `.sha256` checksum file.

- **Windows:** the program is not code-signed yet, so SmartScreen may show "Windows protected your
  PC". Choose *More info*, then *Run anyway*, if you got the file from this project's Releases page.
- **Linux:** needs Ubuntu 24.04 or newer (or Debian 13) with a standard GNOME or KDE desktop. If it
  closes at once without a window on a very minimal install:
  `sudo apt install libxkbcommon-x11-0 libxkbcommon0 libx11-6 libxcursor1 libxi6 libxrandr2 libwayland-client0 libegl1`

## Getting started

1. Start EMAYL and choose **Add account**.
2. Enter your address, then your password. Some providers (for example Yahoo or iCloud) need an
   *app password*, which you create in the provider's account settings; EMAYL tells you when it knows.
3. Gmail and Outlook.com use a **Sign in** button that opens your browser. It appears only in builds
   that include the sign-in registration.

If the program will not start, look for the file `problems.log` in its data folder (on Windows
normally `C:\Users\<you>\AppData\Roaming\emayl\EMAYL\data`). **Settings → Copy report for support**
gives a short report you can send with a bug report.

## Not there yet

Plain-text writing only (no rich text), no signatures, no scheduled sending or snooze, no calendar or
contacts, no automatic updates. More in [`TESTING.md`](TESTING.md).

## Build from source

Needs Rust 1.97 (`rustup`).

```
cargo run -p mailgui --release     # start the program
cargo test --workspace             # run all tests
```

Project layout: `crates/mailcore` (mail engine), `crates/mailgui` (window), `crates/mailcli` (test helper).
Maintainers: see [`MAINTAINING.md`](MAINTAINING.md). Security reports: see [`SECURITY.md`](SECURITY.md).

## License

MIT, see `LICENSE`. (A default choice made by the builder; change it before publishing if you prefer another.)
