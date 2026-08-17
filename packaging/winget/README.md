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
$ver = "0.4.1"
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
winget validate --manifest packaging\winget\0.4.1
```

Then install from the manifest, which is the only check that proves the hash,
the URL and the `ProductCode` all agree with reality:

```powershell
winget install --manifest packaging\winget\0.4.1
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
$dst = "<fork>\manifests\x\Xcomart\Rulogman\0.4.1"
New-Item -ItemType Directory -Force $dst
Copy-Item packaging\winget\0.4.1\*.yaml $dst
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
`.exe`, and lets it compute the hash, bump the version and open the PR:

```
wingetcreate update Xcomart.Rulogman --version <version> --urls <setup.exe url> --submit
```

The job is a no-op unless the **`WINGET_PAT`** repository secret is set — it
logs that it skipped and exits successfully, so a fork or a release made before
the package existed does not fail the build. The token is a **classic** personal
access token with the **`public_repo`** scope, on an account that has forked
winget-pkgs; `wingetcreate` pushes a branch to that fork and opens the PR from
it. Fine-grained tokens do not work here.

Create the secret **only after the first PR has been merged.** Before that
there is nothing for `wingetcreate update` to edit, and the job would fail on
every release for a reason that has nothing to do with the release.

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
its own (`crates/rulogman-app/src/update.rs`) that fetches a release and
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
limitation of the packaging: settings, themes, colour schemes, saved profiles
and `known_hosts` live under `%APPDATA%\aihouse\rulogman`, and passwords and key
passphrases live in the Windows Credential Manager, so neither
`winget uninstall` nor the Inno uninstaller touches them. An uninstall followed
by a reinstall — which is how some upgrade paths behave — keeps your hosts and
keys. Clearing them is a manual deletion of that directory and of the
`com.aihouse.rulogman` entries in Credential Manager.
