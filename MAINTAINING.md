# Keeping EMAYL building: the order of operations

This page is for the person who owns the GitHub repository. Follow it in order and the checks
stay green. Everything here was learned from real failures of this repository.

## What runs by itself

| Part | When it runs | What it does |
|---|---|---|
| **CI** (`ci.yml`) | on every upload to `main`, every pull request, and every Monday | checks formatting, runs the lint and all tests on Linux and Windows, starts the program on a virtual screen, checks the dependency rules (`cargo deny`) |
| **Release** (`release.yml`) | when a version tag such as `v0.2.0` is created | runs CI again, builds the Windows and Linux programs, publishes them as a GitHub release |
| **Dependabot** (`dependabot.yml`) | monthly | proposes updates for the build workflows only (not for the Rust libraries, see rule 2) |

CI builds nothing you can download. Only a release does.

## The three rules

1. **Upload files as one set.** `Cargo.toml` (the library list, in the main folder and in each
   `crates/*` folder) and `Cargo.lock` (the exact versions) must always come from the same batch.
   If they disagree, every check fails with "cannot update the lock file".
2. **Library updates are made together with new code, not by pull requests.** Dependabot is told
   not to open pull requests for Rust libraries, because its edits and a new batch of code kept
   overwriting each other. To update libraries, ask for it when you ask for the next batch; it is
   done and tested together with everything else. Security problems in libraries are still found
   every Monday by the `cargo deny` job: if it turns red, send the log.
3. **Release only from a green commit.** Never create a release while the newest CI run is red.

## A. Uploading a new batch of code

1. Unzip all parts of the batch into **one** empty folder. If asked to merge folders, say yes.
2. Turn on "show hidden files" in your file explorer, so `.github`, `.cargo` and `.gitignore` come along.
3. If the repository already has files from an earlier batch, delete the old `crates` and `vendor`
   folders on GitHub first (open the folder, click the three dots, *Delete directory*, commit).
   GitHub's upload never removes files, so leftovers from older batches would stay and can break the build.
4. **Add file, then Upload files.** GitHub accepts at most 100 files at a time:
   - Upload 1: everything in the folder **except** `crates` (about 46 files). Commit.
   - Upload 2: the whole `crates` folder. Commit.
   - Drag the `crates` folder itself, not the folders inside it, so the paths stay `crates/mailcore/...`.
5. Do not start anything else until step B is finished.

A "changed files only" zip is uploaded the same way: drag the extracted folders and files in; files
with the same name are replaced.

## B. Checking the result (always do this)

1. Open the **Actions** tab. Each upload starts its own CI run, so after two uploads there are two
   runs. Only the **newest** one counts: the earlier one may be red simply because it ran when only half
   of the files were there.
2. If you do not see a new run, click **CI** (left), then **Run workflow**, branch `main`.
3. Wait for the newest run (10 to 20 minutes). Green on all jobs means: the code compiles, all tests pass,
   and the dependency rules hold on Linux and Windows.
4. Red: open the red job, then the red step, and copy its last 30 to 50 lines. If a step named
   *"If a step failed, show what is wrong with the lock file"* ran, copy its text too. Send both.
   Do not click "Re-run" on an old failed run: it repeats the same old files.

## C. Making a release

Only after B is green on the newest commit.

1. Ask for the release files: they are `Cargo.toml` and `Cargo.lock` with the new version number
   (both must change together). Upload them as in A (one commit, both files), then repeat B.
2. On the repository page open **Releases**, then **Draft a new release**.
3. In *Choose a tag* type the version with a leading `v` (for 0.2.0: `v0.2.0`) and pick
   *Create new tag on publish*. The tag must equal `v` plus the version in `Cargo.toml`, or the
   release stops at once.
4. Click **Publish release**. Open the **Actions** tab, then the run named **Release**. It takes about
   15 to 40 minutes: tests, a hidden draft, the two builds, then publishing.
5. When done, the release page lists the Windows installer (`emayl-for-windows-x86_64.exe`), the Windows zip,
   the Linux `.deb`, the Linux `.tar.gz` and a `.sha256` checksum file for each.
6. If the Release run fails: do **not** reuse the tag. Delete the unpublished draft under
   *Releases* if there is one, send me the log, and the next attempt uses the next version number
   (for example 0.2.1), with `Cargo.toml` and `Cargo.lock` changed together.

Releases made this way are not signed, so Windows SmartScreen may warn the first time. Website sign-in
for Google/Microsoft stays hidden until the `EMAYL_*` settings described in section F are added.

## D. Pull requests from Dependabot

Only pull requests for the build workflows (GitHub Actions) can arrive. For each:
1. Wait for its checks. 2. If all are green, click **Merge pull request**. 3. If one is red, close it
(comment `@dependabot close`) and do nothing else. Never merge a pull request that touches
`Cargo.toml` or `Cargo.lock`; close it and tell me instead.

## E. Quick table of known failures

| Message | Cause | What to do |
|---|---|---|
| `cannot update the lock file ... --locked` | `Cargo.toml` and `Cargo.lock` are from different batches (or a library pull request was merged) | upload both files from the newest batch in one commit; read the "show what is wrong" step of the log |
| `unused import` / other warnings only on Windows | Windows-only code was not checked on Linux | send the log; fixed in a changed-files zip |
| Release stops: tag does not match | tag is not `v` + the version in `Cargo.toml` | create a correctly named tag; do not reuse a failed one |
| Build breaks after an upload and an old run is red | an earlier run saw half of the files | check only the newest run (B) |
| `cargo deny` red with no code change | a new security notice about a library | send the log; libraries are updated with the next batch |

## F. Switching on website sign-in (owner only)

This is a one-time job that **cannot be done by the program or its builder**: the providers
require the owner of the program to register it with them. Until it is done, Gmail/Outlook.com
accounts can only be added with an app password (Gmail) or not at all (Outlook.com).
The program reads four optional build settings; leave them empty and the buttons stay hidden.

The screens and wording change over time. The Google facts below were checked against Google's own
help pages on 23 November 2026 (Manage OAuth Clients, Manage App Audience); the Microsoft steps
were not rechecked.

**Google (Gmail), about 10 minutes**
1. Open the Google Cloud Console (console.cloud.google.com) with the Google account that will own the
   registration and create a project (any name, for example EMAYL).
2. Open *Google Auth Platform* and set it up: app name EMAYL, your e-mail as support and contact
   address, audience **External**. Google requires this before a client can be created.
3. Open *Clients → Create client*, choose application type **Desktop app**, and press Create. Copy the
   *client ID* and, if one is shown, the *client secret*. Google shows the secret only once, so save it.
   (Desktop programs cannot keep a secret; Google treats them as public clients.)
4. Open *Audience* and press **Publish app**. Reason (from Google): while the app is in *Testing* only
   up to 100 listed test users can sign in and **Google ends every sign-in after 7 days**, so you would
   have to sign in again every week. In production Google instead shows an **"unverified app"**
   warning for this program, because it asks for the restricted scope `https://mail.google.com/`
   (the only scope that allows IMAP/SMTP), and allows 100 new users in total for the project's
   lifetime. That is plenty for you and your family; verification by Google is only needed beyond that.
   The first sign-in then shows the warning once: choose *Advanced* and then *Go to EMAYL*.
5. In the GitHub repository: *Settings → Secrets and variables → Actions*. Add the **variable**
   `EMAYL_GOOGLE_CLIENT_ID` and the **secret** `EMAYL_GOOGLE_CLIENT_SECRET`.

**Microsoft**
1. In the Microsoft Entra admin center: *App registrations → New registration*. Choose account
   types that include **personal Microsoft accounts** (needed for Outlook.com/Hotmail).
2. Add a platform of type **Mobile and desktop applications** with the redirect address
   `http://localhost`. Allow public client flows.
3. Under *API permissions* add the delegated permissions the program requests:
   `IMAP.AccessAsUser.All` and `SMTP.Send` (Office 365 Exchange Online), plus `offline_access`.
4. Add the **variable** `EMAYL_MICROSOFT_CLIENT_ID` (the *Application (client) ID*). Optionally
   `EMAYL_MICROSOFT_TENANT` (default `common`).

Then publish a release (section C). Settings are read when the program is **built**, so existing
downloads do not change. Do not put these values in files in the repository.

## G. Developer notes

* **Icon:** the artwork is `assets/icon/emayl-source.png`. Run `assets/icon/make-icons.sh`
  (needs Python with Pillow and ImageMagick) to rebuild the PNG sizes, `emayl.ico`,
  `window-64.rgba` and the two installer pictures.
* **Renamed from Rustmail:** on first start the old data and settings folders are moved to the
  new names, and saved passwords are copied from the old password-store entry
  (`dev.rustmail.Rustmail`) to the new one (`dev.emayl.EMAYL`) as each account is opened.
  The Windows installer installs EMAYL next to an old Rustmail; uninstall Rustmail from
  Windows Settings afterwards (your mail is not deleted by that).
* **Environment variables** for the sign-in registrations are now `EMAYL_GOOGLE_CLIENT_ID`,
  `EMAYL_GOOGLE_CLIENT_SECRET`, `EMAYL_MICROSOFT_CLIENT_ID`, `EMAYL_MICROSOFT_TENANT`
  (and `EMAYL_RENDERER=gpu`). Rename your GitHub variables and secrets to match.
* **Installers:** `packaging/windows/installer.nsi` (NSIS) and `packaging/linux/build-deb.sh`.
  The release workflow runs both; the CI workflow checks they still build.

```
cargo test --workspace            # all tests (needs no graphics card or network)
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check                  # dependency rules (cargo install cargo-deny)
cargo run -p mailgui              # start the program
cargo run -p mailcli -- presets   # command-line helper: list built-in providers
```

- `crates/mailcore` — everything except the window: accounts, discovery, IMAP/SMTP, sync, storage,
  safe message rendering, outbox, replies, delivery-failure help.
- `crates/mailgui` — the window (iced). `crates/mailcli` — a small test helper.
- Rust 1.97 (see `rust-version`); CI uses exactly 1.97.0.
- `unsafe` code is forbidden in our crates; release builds abort on panic and check arithmetic overflow.
- Window tests write screenshots to `target/ui-snapshots/`.
