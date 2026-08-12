<#
.SYNOPSIS
    Authenticode-sign one or more Windows artifacts with the release certificate.

.DESCRIPTION
    Authenticode-sign the given files with a self-signed certificate. This is a
    tamper seal, not a reputation: the signing root is trusted by nobody, so
    SmartScreen still shows "Windows protected your PC" and the publisher still
    reads as unknown. It does let users compare the certificate thumbprint
    across releases and it keeps the timestamp, so the signature outlives the
    certificate's own validity window.

    The certificate arrives through the environment rather than as a parameter
    because of a GitHub Actions limitation: the `secrets` context is not
    available in step-level `if:` expressions, so a workflow cannot skip a step
    on "no certificate configured". The secrets are mapped into env instead and
    tested here. A missing certificate therefore skips signing instead of
    failing the build, which keeps forks and pre-secret tags releasable.

    This lives in a script rather than inline in release.yml because there are
    now two things to sign — the executable and the Inno Setup installer built
    around it — and the retry-and-cleanup dance below is not worth duplicating.

.PARAMETER Path
    One or more files to sign. Each is resolved with Resolve-Path, which both
    normalises the separators signtool sees and fails loudly if a build output
    is not where it was expected.

.ENVIRONMENT
    SIGNTOOL     Full path to signtool.exe (the "Locate signtool.exe" step in
                 release.yml finds it inside the Windows SDK and exports it).
    PFX_BASE64   Base64 of the signing PFX. Empty or unset means "skip".
    PFX_PASSWORD Password for that PFX.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0, ValueFromRemainingArguments = $true)]
    [string[]] $Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($env:PFX_BASE64)) {
    Write-Host "signing skipped (no certificate secret)"
    exit 0
}

if ([string]::IsNullOrWhiteSpace($env:SIGNTOOL)) {
    throw "SIGNTOOL is not set; run the 'Locate signtool.exe' step first"
}

# The retry loop below reads $LASTEXITCODE itself, so keep pwsh from throwing
# on the first non-zero signtool exit.
$PSNativeCommandUseErrorActionPreference = $false

$targets = @($Path | ForEach-Object { (Resolve-Path $_).Path })
$pfx = Join-Path $env:RUNNER_TEMP "rulogman-signing.pfx"

try {
    [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String(($env:PFX_BASE64 -replace '\s', '')))
    foreach ($target in $targets) {
        # The timestamp authority is a third-party service; give it a couple of
        # retries before failing the release over a blip.
        $signed = $false
        foreach ($attempt in 1..3) {
            & $env:SIGNTOOL sign /f $pfx /p $env:PFX_PASSWORD /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 $target
            if ($LASTEXITCODE -eq 0) { $signed = $true; break }
            Write-Host "signtool exited $LASTEXITCODE on attempt $attempt of 3"
            if ($attempt -lt 3) { Start-Sleep -Seconds 15 }
        }
        if (-not $signed) { throw "signtool sign failed for $target" }
    }
} finally {
    # The decoded key must not survive the script, failure included.
    Remove-Item $pfx -Force -ErrorAction SilentlyContinue
}

foreach ($target in $targets) {
    # `signtool verify /pa` would fail on purpose here: chain validation ends at
    # an untrusted root. Check that a signature is attached and matches the file
    # instead, which is all a self-signed seal can claim.
    $sig = Get-AuthenticodeSignature $target
    $status = [string]$sig.Status
    if (-not $sig.SignerCertificate) { throw "no signature attached to $target" }
    if ($status -eq 'NotSigned' -or $status -eq 'HashMismatch') {
        throw "authenticode check failed for ${target}: $status"
    }
    # UnknownError is what an untrusted root reports; anything else here is
    # informational only.
    Write-Host "signed $target by $($sig.SignerCertificate.Subject) (status: $status)"
}
