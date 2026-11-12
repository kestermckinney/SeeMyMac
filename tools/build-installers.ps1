param(
    [switch]$Debug
)

# Builds SeeMyMac for ARM64 and 32-bit x86 Windows and packs both into one Inno Setup
# installer (target\installers\SeeMyMac-<version>-setup.exe) that installs the build matching
# the PC's processor. The in-app updater downloads this same file.

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
$rustup = Join-Path $env:USERPROFILE '.cargo\bin\rustup.exe'
$iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
if (-not $iscc) {
    $iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") |
        Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $iscc) { throw 'Install Inno Setup 6 to build installers.' }
if (-not (Test-Path $cargo)) { throw 'Install Rust using rustup.' }
if (-not $env:GPUI_FXC_PATH) {
    $env:GPUI_FXC_PATH = (Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\fxc.exe" -ErrorAction SilentlyContinue |
        Sort-Object { [version]$_.Directory.Parent.Name } -Descending | Select-Object -First 1).FullName
}
if (-not $env:GPUI_FXC_PATH) { throw 'Install the Windows SDK with fxc.exe or set GPUI_FXC_PATH.' }
$llvm = Join-Path $env:ProgramFiles 'LLVM\bin'
if (Test-Path $llvm) { $env:Path = "$llvm;$env:Path" }
# aws-lc-sys (TLS for the RDP gateway) needs NASM to build for 32-bit x86.
if (-not (Get-Command nasm.exe -ErrorAction SilentlyContinue)) {
    $nasm = @("$env:LOCALAPPDATA\bin\NASM", "$env:ProgramFiles\NASM", "${env:ProgramFiles(x86)}\NASM") |
        Where-Object { Test-Path (Join-Path $_ 'nasm.exe') } | Select-Object -First 1
    if (-not $nasm) { throw 'Install NASM (winget install NASM.NASM) to build for x86.' }
    $env:Path = "$nasm;$env:Path"
}
$metadata = & $cargo metadata --manifest-path (Join-Path $root 'Cargo.toml') --locked --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
$version = ($metadata.packages | Where-Object { $_.name -eq 'see_my_mac' } | Select-Object -First 1).version
$targets = [ordered]@{ arm64 = 'aarch64-pc-windows-msvc'; x86 = 'i686-pc-windows-msvc' }
$profile = if ($Debug) { 'debug' } else { 'release' }
$binaries = @{}
Push-Location $root
try {
    foreach ($arch in $targets.Keys) {
        $target = $targets[$arch]
        & $rustup target add $target
        if ($LASTEXITCODE -ne 0) { throw "Could not install $target." }
        $cargoArgs = @('build', '--locked', '--bin', 'see_my_mac', '--target', $target)
        if (-not $Debug) { $cargoArgs += '--release' }
        & $cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) { throw "Build failed for $target." }
        $binaries[$arch] = Join-Path $metadata.target_directory "$target\$profile\see_my_mac.exe"
        if (-not (Test-Path $binaries[$arch])) { throw "Missing $($binaries[$arch])." }
    }
    $suffix = if ($Debug) { '-debug' } else { '' }
    & $iscc "/DAppVersion=$version" "/DBinaryArm64=$($binaries.arm64)" "/DBinaryX86=$($binaries.x86)" "/DBuildSuffix=$suffix" (Join-Path $root 'packaging\windows\seemymac.iss')
    if ($LASTEXITCODE -ne 0) { throw 'Installer creation failed.' }
} finally { Pop-Location }
