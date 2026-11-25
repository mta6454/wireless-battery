# Builds dist\WirelessBattery-Setup-<version>.exe and dist\WirelessBattery-<version>-portable.zip
# (the Windows installer and portable zip of the cross-platform port).
# Needs: the release exe already built (cargo build --release) and Inno Setup 6 or 7 installed.
# Run it from any folder; set $env:WB_VERSION to the same version before `cargo build` so the exe's
# version resource matches.
param([string]$Version = "1.0.0")
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root

# Bundled HeadsetControl: pinned release, verified against the SHA-256 GitHub publishes for it.
$hscVersion = "4.3.0"
$hscSha256  = "e371da043710ddb8fc70af77f2ccad1cd03f8c6f49ba423e08764ad7a94a3c40"
$hscDir = Join-Path $root "third_party\headsetcontrol"
$hscExe = Join-Path $hscDir "headsetcontrol.exe"
New-Item -ItemType Directory -Force $hscDir | Out-Null
if (-not (Test-Path $hscExe) -or (Get-FileHash $hscExe -Algorithm SHA256).Hash -ne $hscSha256) {
    Write-Host "Downloading HeadsetControl $hscVersion..."
    Invoke-WebRequest "https://github.com/Sapd/HeadsetControl/releases/download/$hscVersion/headsetcontrol-windows-x86_64.exe" -OutFile $hscExe
}
if ((Get-FileHash $hscExe -Algorithm SHA256).Hash -ne $hscSha256) {
    Remove-Item $hscExe
    throw "HeadsetControl checksum mismatch - refusing to bundle it."
}

$exe = Join-Path $root "target\release\wireless-battery.exe"
if (-not (Test-Path $exe)) { throw "Build the exe first: cargo build --release" }

$iscc = foreach ($v in 7, 6) {
    foreach ($base in ${env:ProgramFiles(x86)}, $env:ProgramFiles, "$env:LOCALAPPDATA\Programs") { "$base\Inno Setup $v\ISCC.exe" }
}
$iscc = $iscc | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { $iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source }
if (-not $iscc) { throw "Inno Setup not found (https://jrsoftware.org/isdl.php)." }
& $iscc "/DAppVersion=$Version" (Join-Path $PSScriptRoot "WirelessBattery.iss")
if ($LASTEXITCODE -ne 0) { throw "ISCC failed." }

# Portable zip: same files, no installer.
$stage = Join-Path $root "build\portable"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $exe (Join-Path $stage "WirelessBattery.exe")
Copy-Item $hscExe, installer\THIRD_PARTY_NOTICES.txt, installer\GPL-3.0.txt $stage
$licenses = Join-Path $root "installer\THIRD_PARTY_LICENSES.txt"   # made by scripts/third-party-licenses.py
if (Test-Path $licenses) { Copy-Item $licenses $stage } else { Write-Warning "installer\THIRD_PARTY_LICENSES.txt not found: Rust crate licenses will be missing from the package." }
Copy-Item LICENSE (Join-Path $stage "LICENSE.txt")
New-Item -ItemType Directory -Force dist | Out-Null
Compress-Archive -Path "$stage\*" -DestinationPath "dist\WirelessBattery-$Version-portable.zip" -Force
Write-Host "Done: dist\WirelessBattery-Setup-$Version.exe and dist\WirelessBattery-$Version-portable.zip"
