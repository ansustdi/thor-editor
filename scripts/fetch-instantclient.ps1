# Downloads Oracle Instant Client "Basic Light" (Windows x64) into src-tauri/resources/instantclient,
# which the installer bundles so Oracle works on PCs without an Oracle Client installed.
# Oracle Instant Client is free to use and redistribute under the Oracle Free Use Terms and Conditions
# (see LICENSE in the downloaded folder).
#
#   npm run fetch:oracle            # latest version
#   npm run fetch:oracle -- -Force  # re-download even if present
param(
    [string]$Url = "https://download.oracle.com/otn_software/nt/instantclient/instantclient-basiclite-windows.zip",
    [switch]$Force
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root "src-tauri\resources\instantclient"

if ((Test-Path (Join-Path $dest "oci.dll")) -and -not $Force) {
    Write-Host "Instant Client already present in $dest (use -Force to re-download)."
    exit 0
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("instantclient-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    $zip = Join-Path $tmp "ic.zip"
    Write-Host "Downloading $Url ..."
    Invoke-WebRequest -Uri $Url -OutFile $zip -UseBasicParsing
    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $src = Get-ChildItem $tmp -Directory | Where-Object { $_.Name -like "instantclient_*" } | Select-Object -First 1
    if (-not $src) { throw "Unexpected archive layout: no instantclient_* folder" }

    if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
    New-Item -ItemType Directory -Force $dest | Out-Null
    Copy-Item -Path (Join-Path $src.FullName "*") -Destination $dest -Recurse -Force
    Set-Content -Path (Join-Path $dest "VERSION.txt") -Value $src.Name
    $mb = [math]::Round(((Get-ChildItem $dest -Recurse | Measure-Object Length -Sum).Sum / 1MB), 1)
    Write-Host "Installed $($src.Name) into $dest ($mb MB)."
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
