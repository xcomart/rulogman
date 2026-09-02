; Inno Setup script for the Windows installer.
;
; Why an installer at all, when the zip already works. The zip is what the
; in-app updater downloads and unpacks (crates/rulogman-app/src/update.rs), so
; it is not going anywhere. What it cannot do is register the program with
; Windows: unzipping leaves no entry under "Apps & features" (the ARP keys
; below HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall), and winget
; reads exactly those keys to decide which version is installed, whether an
; upgrade is available, and how to remove it. A package whose installer leaves
; no ARP entry is rejected by winget-pkgs validation and, if it slipped
; through, would report "no applicable upgrade" forever. So the installer
; exists for winget, and the zip stays for the updater.
;
; What it installs is genuinely just an executable. rulogman.exe is
; self-contained — the SSH client, the terminal emulation and the renderer are
; all linked into it, and everything it reads at runtime (settings, themes,
; profiles, known_hosts) it creates for itself under %APPDATA%\aihouse\rulogman
; on first launch. There is no runtime tree to lay down beside it and no
; ordering constraint to get wrong, which is why this script is as short as it
; is. The one thing it must keep doing is writing the uninstall key above.
;
; Compiled from CI with:
;
;   ISCC.exe /DVersion=0.5.5 ^
;            /DSourceDir=<staging tree> ^
;            /DOutputDir=<where the .exe lands> ^
;            /DOutputBaseFilename=rulogman-v0.5.5-x86_64-pc-windows-msvc-setup
;
; Version carries no "v" prefix — VersionInfoVersion is a numeric quad and
; rejects one.

#ifndef Version
  #error Version is required: pass /DVersion=X.Y.Z
#endif
#ifndef SourceDir
  #error SourceDir is required: pass /DSourceDir=<staged tree>
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
#ifndef OutputBaseFilename
  #define OutputBaseFilename "rulogman-setup"
#endif

[Setup]
; This GUID is a published identifier, not an implementation detail. Inno
; derives the uninstall registry key from it, winget records it as the
; package's ProductCode, and both an upgrade in place and `winget uninstall`
; find the existing install by matching it. Changing it would orphan every
; copy already on disk — the old entry would linger in "Apps & features" with
; no way to remove it and the new one would install alongside. It never
; changes. The doubled leading brace is Inno's escape for a literal "{".
AppId={{D6066CD8-5F5D-4B13-AB5B-DAD7965FF725}
AppName=rulogman
AppVersion={#Version}
VersionInfoVersion={#Version}
AppPublisher=Xcomart
AppPublisherURL=https://github.com/xcomart/rulogman
AppSupportURL=https://github.com/xcomart/rulogman
AppUpdatesURL=https://github.com/xcomart/rulogman

; Per-user install, deliberately. PrivilegesRequired=lowest means no UAC
; prompt and no elevation, which is what winget's default (unelevated) install
; flow wants and what lets the app update itself later without asking for
; administrator rights — the updater replaces the executable in place, and it
; can only do that in a directory the user owns. Under "lowest", {autopf}
; resolves to %LOCALAPPDATA%\Programs and {autoprograms} to the per-user Start
; menu, so the same script would also do the right thing if it were ever run
; elevated.
PrivilegesRequired=lowest
DefaultDirName={autopf}\rulogman
; There is exactly one shortcut and it is not in a folder of its own, so the
; "Select Start Menu Folder" page has nothing to ask about. [Icons] names
; {autoprograms} directly rather than going through {group}.
DisableProgramGroupPage=yes

; gpui renders through DirectX and the build only targets x86_64-pc-windows-msvc;
; there is no 32-bit or ARM artifact to fall back to. x64compatible rather than
; x64 so the installer also runs under the x64 emulation layer on ARM64
; Windows, where the same binaries do work.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

; One large executable and a README. lzma2/max is worth the compile time on a
; binary that carries a shader blob and a font's worth of static data;
; SolidCompression is not, because it only pays across many small files and
; there are two files here in total.
Compression=lzma2/max
SolidCompression=no
WizardStyle=modern

; Paths are relative to this script, which lives in packaging\windows\.
SetupIconFile=..\..\assets\icon.ico
LicenseFile=..\..\LICENSE
UninstallDisplayIcon={app}\rulogman.exe
UninstallDisplayName=rulogman

OutputDir={#OutputDir}
OutputBaseFilename={#OutputBaseFilename}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; Unchecked: a desktop icon is an opinion, and a silent winget install (which
; passes /VERYSILENT and therefore accepts every default) should not litter
; the desktop of someone who only typed `winget install`.
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; One recursive entry rather than a line per file, even though the payload is
; small enough to enumerate. What ships is whatever the "Package (windows)"
; step staged, which is the same tree the zip carries — so the installer and
; the zip cannot drift apart, and a file added to the release later needs no
; edit here.
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\rulogman"; Filename: "{app}\rulogman.exe"
Name: "{autodesktop}\rulogman"; Filename: "{app}\rulogman.exe"; Tasks: desktopicon

[Registry]
; The rulogman:// URL scheme, which is how anything that can open a URL — a
; `start rulogman://dashboard/Morning` at the prompt, a link clicked in a
; browser or a chat window — asks rulogman for a saved dashboard by name.
; Windows resolves a scheme by looking it up as a class: the "URL Protocol"
; value marks the class as a protocol (it is empty, and it is its presence that
; counts), the default value is the description, and shell\open\command is what
; runs — "%1" hands the whole URL over as a single argument, which is where
; rulogman reads the dashboard name from.
;
; HKCU rather than HKLM because the install is per-user (PrivilegesRequired=lowest
; above): a machine-wide class registration would need exactly the elevation this
; installer deliberately never asks for, and a per-user registration is the right
; one for an application living under the user's own %LOCALAPPDATA%. It is also
; the half Windows prefers when both exist. uninsdeletekey on the first line
; takes the whole key back out on uninstall, so no scheme is left pointing at an
; executable that has gone.
Root: HKCU; Subkey: "Software\Classes\rulogman"; ValueType: string; ValueName: ""; ValueData: "URL:rulogman"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\rulogman"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\rulogman\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\rulogman.exe"",0"
Root: HKCU; Subkey: "Software\Classes\rulogman\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\rulogman.exe"" ""%1"""

[Run]
Filename: "{app}\rulogman.exe"; Description: "{cm:LaunchProgram,rulogman}"; Flags: nowait postinstall skipifsilent

; No [UninstallDelete] section, and that is a decision rather than an omission.
; Uninstalling removes only what was installed: settings, themes, colour
; schemes, the saved profiles, the known_hosts file and the Windows Credential
; Manager entries holding passwords and key passphrases all live outside {app}
; (under %APPDATA%\aihouse\rulogman and in the credential store) and are left
; untouched. That is what makes an uninstall followed by a reinstall — which is
; how some upgrade paths behave — keep a user's hosts and keys instead of
; silently wiping them.
