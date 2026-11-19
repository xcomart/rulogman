# winget manifests

These are the manifests that put rulogman in the Windows Package Manager, so that
`winget install Xcomart.Rulogman` works. They live here rather than only in
[microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) because the
copy in this repository is the source they are edited from: the community
repository is where a *copy* of them is published, one directory per version,
and a manifest that only exists over there has no history next to the `.iss`
file, the release workflow and the `AppId` it depends on.

What winget registers is the **Inno Setup installer**, not the plain zip. The
zip unpacks a folder and registers nothing with Windows, and winget identifies
an installed package by its "Apps & features" registry entry — no entry, no
package it can find, upgrade or uninstall. `packaging/windows/rulogman.iss`
writes that entry; these manifests tell winget where to download it and what
the entry will look like when it lands.

Each version gets a directory named after it, holding the three files winget
requires:

| File | What it carries |
|---|---|
| `Xcomart.Rulogman.yaml` | The version manifest — the identifier, the version, and which locale is the default |
| `Xcomart.Rulogman.installer.yaml` | The download URL, its SHA256, the installer type and scope, and the `ProductCode` |
| `Xcomart.Rulogman.locale.en-US.yaml` | Everything a human reads: publisher, license, description, tags |

The filenames are not decorative. winget-pkgs requires that they be
`<PackageIdentifier>.yaml`, `<PackageIdentifier>.installer.yaml` and
`<PackageIdentifier>.locale.<locale>.yaml`, so the directory here can be copied
into a fork verbatim.

The installer manifest also declares a dependency on
`Microsoft.VCRedist.2015+.x64`. `rulogman.exe` links the MSVC runtime
dynamically, and neither the installer nor the zip carries it, so a machine
without that redistributable already installed fails to launch it at all —
which is what happened on the winget-pkgs validation VM before the release
build started statically linking the CRT (see `.github/workflows/release.yml`).
The dependency stays declared as a safety net even so, since it costs nothing
on a machine that already has the runtime. Because `wingetcreate update` edits
a copy of the previously merged manifest rather than regenerating one from
scratch, this declaration carries forward to every release automatically —
nothing here needs to repeat it by hand when bumping the version below.

Releases carry an ARM64 installer beside the x64 one
(`rulogman-v<version>-aarch64-pc-windows-msvc-setup.exe`), so the installer
manifest lists two installers, `x64` and `arm64`, and winget picks the native
one. `wingetcreate update` only updates the installers a manifest already
has, so the release workflow's `winget` job adds the `arm64` entry itself for
the first release that has one — with the same `ProductCode`, and
`Microsoft.VCRedist.2015+.arm64` as its own dependency in place of the x64
one — and hands `wingetcreate` both URLs from then on.

The manifests are written against **manifest schema 1.12.0**, which is the
newest schema winget-pkgs actually merges against.

## Registering the package for the first time

`wingetcreate update`, which the release workflow runs, can only *edit* a
package that already exists. The first submission has to be made by hand, once.

**1. Fork winget-pkgs** and clone the fork.

```powershell
gh repo fork microsoft/winget-pkgs --clone --remote
```

**2. Fill in the placeholders.** The installer manifest ships with a hash of 64
zeros and a `ReleaseDate` of `1970-01-01`, because neither can be known before
the release exists. Download the asset the manifest points at and hash it:

```powershell
$ver = "0.5.9"
$url = "https://github.com/xcomart/rulogman/releases/download/v$ver/rulogman-v$ver-x86_64-pc-windows-msvc-setup.exe"
Invoke-WebRequest -Uri $url -OutFile "$env:TEMP\rulogman-setup.exe"
(Get-FileHash -Algorithm SHA256 "$env:TEMP\rulogman-setup.exe").Hash
```

Paste the hash into `InstallerSha256` and the release's publication date into
`ReleaseDate`. Keep the hash uppercase — that is what every other manifest in
the repository uses, and what `wingetcreate` will write on the next release, so
matching it now keeps the diff of the first automated update honest.

**3. Validate locally, before the PR.** Schema validation is instant and catches
every typo the CI would otherwise catch twenty minutes later:

```powershell
winget validate --manifest packaging\winget\0.5.9
```

Then install from the manifest, which is the only check that proves the hash,
the URL and the `ProductCode` all agree with reality:

```powershell
winget install --manifest packaging\winget\0.5.9
```

That second command needs **Developer Mode** turned on (Settings → System →
For developers) — winget refuses local manifests without it. Afterwards,
confirm winget can actually see what it installed, because this is precisely
what a wrong `ProductCode` breaks:

```powershell
winget list Xcomart.Rulogman
winget uninstall Xcomart.Rulogman
```

**4. Copy into the fork and open the PR.** The path is derived from the
identifier: first letter of the publisher, then publisher, then package, then
version.

```powershell
$dst = "<fork>\manifests\x\Xcomart\Rulogman\0.5.9"
New-Item -ItemType Directory -Force $dst
Copy-Item packaging\winget\0.5.9\*.yaml $dst
```

Commit on a branch and open the PR against `microsoft/winget-pkgs`.

**5. Wait.** A bot validates the manifest and installs the package in a sandbox.
For an existing package that is usually all that happens and the merge is
automatic. A **new** package also draws a human reviewer, so the first PR can
sit for several days — that is normal, and the answer is patience rather than a
second PR.

## Every release after the first

The `winget` job in
[`.github/workflows/release.yml`](../../.github/workflows/release.yml) submits
the update. It downloads `wingetcreate`, points it at the new release's setup
`.exe`, and lets it compute the hash and bump the version into a local
directory, then adds the release notes and submits that directory:

```
wingetcreate update Xcomart.Rulogman --version <version> --urls <setup.exe url> \
  --release-notes-url <release url> --release-date <YYYY-MM-DD> --out <dir>
wingetcreate submit --prtitle "New version: Xcomart.Rulogman version <version>" --no-open <manifest dir>
```

The step in between is for `ReleaseNotes`. `wingetcreate update` clears the
fields that belong to one release from the manifests it copies and never fills
`ReleaseNotes` back in, while winget-pkgs' validator compares every submission
with the published version and holds the pull request with the
`Manifest-Metadata-Consistency` label when a field that version had is
missing. That is what happened to rudbman: its 0.3.7 went in through the bot
below with the release body as its notes, so 0.3.8 was held for `Missing
property ReleaseNotes` until they were added by hand. The job now turns the
published GitHub release body into plain text — no emphasis or code marks,
`#N` for this repository's pull requests, whole lines up to winget's
10,000-character limit — and writes it into the locale manifest as
`ReleaseNotes` before it submits.

The notes reach `awk` through an environment variable rather than `awk -v`,
because `-v` expands backslash escapes and the Windows runner's temporary
directory is `D:\a\_temp`: the `\a` turned into a bell character, the notes
file could not be opened, and rudbman 0.3.9 and rulogman 0.6.6 went out with
`ReleaseNotes: ""` and had to be fixed on their pull requests. Whatever
`ReleaseNotes` wingetcreate wrote is replaced, and the job now fails rather
than submit when the release body yields no notes, so an empty field cannot
reach winget-pkgs again.

Before that, the job looks at what winget-pkgs already has. When the version
is already merged there, or an open pull request names it in its title, the job
skips the submission rather than open a second pull request for the same
version. That is not hypothetical: a third-party bot (pl4nty's winget-recipes)
watches GitHub releases and opened the pull request for rudbman 0.3.7 on its
own, and rerunning the job would otherwise meet its own earlier pull request.

Nothing is submitted unless the **`WINGET_PAT`** repository secret is set.
Without it the job leaves a warning on the run and exits successfully, so a
fork does not fail the build — but a green `winget` job carrying that warning
means no pull request was opened. The token is a **classic** personal access
token with the **`public_repo`** scope, on an account that has forked
winget-pkgs (`xcomart/winget-pkgs`); `wingetcreate` pushes a branch to that
fork and opens the PR from it. Fine-grained tokens do not work here. Set it
once, and paste the token when asked:

```
gh secret set WINGET_PAT --repo xcomart/rulogman
```

The package is registered already, so the secret can be set at any time;
before the first PR had been merged there was nothing for `wingetcreate update`
to edit, and the job would have failed on every release. A token with an
expiry date stops working when it expires: the submission step then fails on
authentication, and the secret needs a new token.

The submission can also fail with "The forked repository could not be synced
with the upstream commits", which happened on rudbman 0.3.8 when the fork had
fallen thousands of commits behind. The fork carries no commits of its own, so
a fast-forward fixes it; then rerun the failed job:

```
gh repo sync xcomart/winget-pkgs --source microsoft/winget-pkgs --branch master
gh run rerun <run-id> --failed
```

The manifests here do not update themselves when that job runs. Copying the
merged version back into a new directory in this folder is a manual step, and
worth doing: it keeps this directory the readable record of what was published.

## Bumping the version

Copy the newest directory to one named after the new version and change four
things, in three files:

- `PackageVersion` — in **all three** files. winget rejects the set if they
  disagree.
- `InstallerUrl` — the tag and the version both appear in the asset name.
- `InstallerSha256` — recomputed from the new asset.
- `ReleaseDate` — and `ReleaseNotesUrl` in the locale manifest, which carries
  the tag.

**`ProductCode` does not change, and must not.** It is
`packaging/windows/rulogman.iss`'s `AppId` with `_is1` appended, the suffix Inno
Setup adds when it writes its uninstall registry key. It is the only thing that
lets winget match an installed copy of rulogman to this package — which is why
`AppId` in `rulogman.iss` is frozen, and why the same GUID is spelled out a
third time in `crates/rulogman-app/src/update.rs`. Change it and every existing
installation becomes invisible to `winget upgrade` and `winget uninstall`,
silently, and installing the new version leaves the old one behind in Apps &
features with no way to remove it through winget. There is no migration path;
the only fix is never to do it.

## Known limitations

**SmartScreen still warns.** Installing through winget changes nothing about
the signature on the executable, which is self-signed at best. The Inno
installer will still raise "Windows protected your PC" on a machine that has
not seen it before. winget is a distribution channel, not a trust anchor.

**The built-in updater keeps the version in step.** rulogman has an updater of
its own (`rugpui-shell`'s, told which uninstall key is rulogman's by
`crates/rulogman-app/src/update.rs`) that fetches a release and
replaces the installed executable in place, and winget's record of the installed
version is the `DisplayVersion` under the uninstall key Inno wrote. Those would
drift apart — `winget list` reporting a version that has not been on disk for
months, `winget upgrade` offering a release already applied — so the updater
writes the new version into that key itself, right after the swap succeeds. It
only ever writes to an entry whose `InstallLocation` is the directory the
running executable is actually in, so a portable copy unpacked from the zip
neither creates an entry nor touches the installed copy's. `HKEY_CURRENT_USER`
is tried first, since that is where a per-user install lands; a copy installed
elevated is found under `HKEY_LOCAL_MACHINE` instead, and an unelevated process
that cannot write there simply leaves the value alone rather than failing the
update.

**Uninstalling right after a self-update can leave two files behind.** Windows
will not delete a running executable but will rename one, so the updater
renames the old `rulogman.exe` aside as `rulogman.exe.old` and clears it on the
next launch, and it unpacks into a `.update` directory beside the installed
copy. Neither name is in Inno's uninstall log — that log was written at install
time and lists what the installer put down — so an uninstall performed before
rulogman has been started once more removes everything it knows about and leaves
the directory in place because it is not empty. Removing what is left is a
manual `Remove-Item`. Reinstalling over the top is unaffected.

**Uninstalling keeps your profiles.** That is deliberate rather than a
limitation of the packaging: settings, themes, color schemes, saved profiles
and `known_hosts` live under `%APPDATA%\aihouse\rulogman`, and passwords and key
passphrases live in the Windows Credential Manager, so neither
`winget uninstall` nor the Inno uninstaller touches them. An uninstall followed
by a reinstall — which is how some upgrade paths behave — keeps your hosts and
keys. Clearing them is a manual deletion of that directory and of the
`com.aihouse.rulogman` entries in Credential Manager.
