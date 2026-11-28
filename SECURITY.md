# Security

## Reporting a problem

Please do **not** open a public issue for a security problem.

Use GitHub's private reporting: on the repository page choose **Security → Report a vulnerability**.
(The repository owner must have *Private vulnerability reporting* switched on under
*Settings → Code security*; if the button is missing, it is not on yet.)

Include what you did, what you expected, what happened, and the version shown on the
program's **Settings** screen. Do not include real passwords or private mail.
This is a small volunteer-style project: reports are answered on a best-effort basis and
no response time is promised.

Only the **latest release** is looked after.

## What the program is designed to protect against

| Threat | What the program does |
|---|---|
| Someone listening on the network | TLS 1.2 or newer for every mail connection; certificates are checked with the operating system's rules; no option to skip checks; no plain-text fallback. |
| A downgrade to an unencrypted connection | Only two connection modes exist: encrypted from the first byte, or "upgrade with STARTTLS" where the program refuses to continue if the upgrade fails. There is no unencrypted mode. |
| Passwords stolen from disk | Passwords and sign-in tokens live only in the OS password store (Windows Credential Manager, Linux Secret Service). They are never written to the program's own files or support report. |
| Malicious message content | HTML is rewritten to a small safe subset; scripts, styles, forms and remote content are removed or not loaded. Links are checked and shown in a confirmation dialog before they are opened. |
| Tracking by "read receipts" or pictures | Nothing is fetched from the internet when a message is shown, so tracking pixels never reach their sender. The reading page counts what was held back. Known tracking codes (`utm_*`, `fbclid`, `gclid`, and a fixed list of others) are removed from links before they are opened or copied. Only codes on that list are recognised; a sender can still tell which link was clicked if the address itself is unique to you. |
| Dangerous attachments | Attachments are never opened by the program, only saved on request (never overwriting a file; Windows "mark of the web" is added). Programs and disguised programs (for example `photo.jpg.exe`) need an extra confirmation. |
| Header injection when writing | Addresses, subjects and file names containing control characters (such as line breaks) are rejected before a message is built. |
| Hostile discovery data | Provider settings are fetched only over HTTPS, size- and time-limited, parsed without external entities, and shown to you before the first sign-in. |
| A hostile mail server sending endless data | Every IMAP answer is counted; if one answer exceeds the size the program asked for (plus 1 MiB; at most 48 MiB for any other answer) the connection is dropped. |
| Settings files that point the program at other services | Server settings found on the internet may only use the usual mail ports (IMAP 143/993; SMTP 25/465/587/2525) and may not name machines that only exist inside a local network (`.local`, `.internal`, `.lan`, `localhost`, …). Settings typed in by the person are not restricted. |
| Mail left behind after removing an account | Removing an account deletes its saved password, its downloaded mail, its unsent messages and its drafts, overwrites freed database pages, and a start-up sweep removes anything left from an earlier crash. |
| Other users of the same computer | Mail, outbox, drafts, the database, and the small state files (window size, start-up notes, problem log) are created readable only by you (Linux; older looser files are tightened); loose permissions on existing folders are tightened; files are never written through a pre-planted link; leftover temporary files are removed at start-up. |
| Spoofed or disguised links and file names | Links with a hidden login (`https://bank.com@evil.example`), look-alike alphabets, text-direction tricks, or non-web schemes are flagged or refused; only `http(s)` addresses without spaces or control characters are ever given to the operating system. Windows reserved file names (`CON`, `NUL`, `COM1`, also `CON .txt` and `COM¹`) are renamed when saving an attachment. |
| Information leaking out in sent mail | Sent messages carry only the standard headers (a test lists them): no program name, computer name, IP address or read-receipt request. The program never sends read receipts. |
| Two copies of the program at once | A lock on the mail folder lets only one copy run; a second one shows a plain "already open" message. This stops duplicate sending and lost account changes. The lock is released by the operating system, even after a crash. |
| Cancel/send and discard/autosave races | The outbox and the drafts serialize their changes, so "Cancel" and "start sending" cannot both succeed, and a background save cannot bring back a draft that was discarded or sent. |
| Corrupt or hostile local database | SQLite is built without extension loading, with strict quoting rules (see `.cargo/config.toml`), and has a test that checks those options took effect. |
| Risky dependencies | `deny.toml` is enforced in CI: known security notices fail the build; OpenSSL, aws-lc and GTK are banned; only crates.io is allowed as a source; only listed licenses are allowed. A weekly CI run catches new notices. |
| Tampered release builds | Releases are built by GitHub Actions from the tagged source with a pinned compiler and no build cache; `unsafe` is forbidden in our code; checksums are published. |

## What it does **not** protect against

- **Malware that is already running as you.** A virus or trojan running under your own account
  can read your files, watch the screen and ask the operating system's password store for the same
  secrets this program uses; the Windows Credential Manager and the Linux Secret Service trust the
  whole user account, not one program. No mail program can prevent that without help from the
  operating system. What this program does is make itself a poor *way in*: it is written in a
  memory-safe language with `unsafe` forbidden, it never runs anything from a message (no scripts,
  no macros, attachments are only saved, never opened), its parsers have size and depth limits, and
  it opens links only after a confirmation. Keep the operating system and an up-to-date antivirus,
  and use disk encryption.

- **Anyone with access to your unlocked computer or your user account.** Mail stored on disk
  is **not encrypted** by the program. Use disk encryption.
- **A compromised release pipeline or repository account.** The checksum files come from the same
  place as the downloads; they find damaged files, not malicious ones. There is no code signing
  on Windows and no build provenance attestation yet.
- **Weaknesses in the C/assembly parts** (the *ring* crypto library and bundled SQLite), beyond
  keeping them current.
- **A malicious mail server wasting time or bandwidth** (the memory it can make the program use for one answer is capped, but a slow or chatty server can still be annoying until the connection times out).
- **Message signing and encryption** (S/MIME, PGP): not supported, so nothing is verified or decrypted.
- **Flaws that only real-world use reveals.** The program has not been audited by an independent
  security reviewer and has not been used with real accounts by its authors.

## Hardening pass (2026-11-20) — what it was and was not

A review of the program's own code, done by its builder (an AI assistant), not by an independent
person. It read the code paths that handle outside data, fixed what it found, and added a test for
each fix. It is **not** a penetration test of a running installation, not a substitute for an audit,
and found only what it knew to look for.

Fixed: files created through a pre-planted link or with loose permissions; mail, unsent messages and
drafts left behind after removing an account; deleted text staying readable in database files;
unbounded IMAP answers; settings files able to name arbitrary ports or local-network names; spaces
inside `mailto:`/`tel:` links; a search containing a NUL character failing; two program copies sharing one outbox; a cancelled message returning as "sending"; discarded drafts coming back from a late autosave; text-direction tricks in sign-in error text; two Windows file-name
edge cases.

Checked by reading the code and found already handled (no change): SMTP dot-stuffing, line endings,
reply-size limits and STARTTLS leftover-data check; address characters that could break out of SMTP
commands; deeply nested HTML/MIME; password fields masked and cleared after use; secrets held in
`secrecy` types; no log file is written (the few internal log lines have no log reader attached, so they go nowhere).

Not done and why: **no ban on connecting to private IP addresses at run time** (people legitimately
run mail servers at home or at work; only settings found on the internet are restricted);
**no protection against memory inspection or core dumps** (needs `unsafe` code, which is forbidden);
**no guarantee that deleted data is gone from the disk itself** (SSD wear-levelling and file-system
journals are outside the program's control; use disk encryption); typed passwords may remain in
memory copies made by the window toolkit.

## Round of 2026-11-26 (rename, appearance, trackers, unread badge)

Changes with a security angle, by the same builder and with the same limits as above:

- The first start after the rename moves the old data folders and copies saved passwords to the new
  password-store name; a secret is removed from the old entry only after the new one holds it.
- The three small state files are now written owner-only (they were created with the default
  rights, inside the already private folder).
- The unread count is drawn into the window icon and sent to the Linux dock over the user's own
  session bus. Neither contains message text, and nothing goes over the internet. The Windows
  taskbar "overlay" feature was **not** used because it needs `unsafe` system calls that this
  program forbids itself; the taskbar shows the badged icon and the title "(n) EMAYL" instead.
- Considered and **not** done: encrypting the local mail copy with a key from the password store
  (SQLCipher is not available to the build; doing it by hand for message bodies only would leave
  subjects, senders and the search index readable and risk losing mail), and Windows process
  hardening calls (they need `unsafe` and could not be compiled or tested here).

## Round of 2026-11-26 (batch 5: filters, templates, undo send)

- Blocked senders, rules and templates are kept in their own small files (`rules.json`,
  `templates.json`) in the program's private data folder, written the same way as the other small
  files (owner-only, never through a planted link). They are *data*, never code: rules have no
  regular expressions or scripts, and need at least three characters so a typo cannot match every
  message; a damaged file is reported and never overwritten.
- Rules read only header fields that are already stored (sender, subject, recipients). Moving mail
  to Junk or the archive is a *move*, never a deletion, and goes through the same queued,
  retried server commands as the buttons.
- *Block* refuses your own addresses.
- A message waiting for "undo send" is already on disk in the outbox like any queued message (so
  it survives a crash and is sent on the next start); *Undo* removes it exactly as *Cancel* in the
  Outbox does. While it waits, the text is also held in memory so *Undo* can reopen it; that copy
  is dropped when the time is up.
- Not done: **no content-based spam detection** (it would need either looking inside message text
  with a statistical model, or sending data to an online service; neither was added).

## Round of 2026-11-27 (batch 6: long lists, layout choices)

- New saved choices (`side_by_side`, `list_width`) are read back only if they are one of the offered values; anything else is ignored. They are not secrets and live in the same small file as the other look choices.
- The list now builds only part of the rows; it handles the same data as before and adds no new input. Scroll positions from the window are only used as numbers to pick a range, with unit tests for NaN, infinity and out-of-range values.
- Nothing else in this round touches mail, accounts, the network or stored secrets.

## Known accepted dependency notices

`deny.toml` lists, with reasons, the "unmaintained"/"unsound" notices that are accepted for now
(chrono, paste, pin-utils, rustybuzz, ttf-parser, zbus_names, zvariant, zvariant_derive and lru).
They are to be reviewed at the latest on **2027-02-19**.

The bundled SQLite (3.54.1 at the time of writing) is newer than the shell-only `.recover` command
problem found in a published SQLite notice; the program only uses the library, never the SQLite
command-line shell. (This was read from one third-party tracker page, not from SQLite's own advisory.)

## Safe to share in a bug report

The **Copy report for support** text (Settings screen) and the **Check connection** report contain
version, system, folder paths, server names and status. They contain no passwords, tokens, message
text or email addresses. Look over them before posting anyway.
