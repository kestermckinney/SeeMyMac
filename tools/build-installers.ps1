param(
    [ValidateSet('arm64', 'x86', 'all')]
    [string]$Architecture = 'all',
    [switch]$Debug
)

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
$metadata = & $cargo metadata --manifest-path (Join-Path $root 'Cargo.toml') --locked --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed.' }
$version = ($metadata.packages | Where-Object { $_.name -eq 'see_my_mac' } | Select-Object -First 1).version
$targets = @{ arm64 = 'aarch64-pc-windows-msvc'; x86 = 'i686-pc-windows-msvc' }
$arches = if ($Architecture -eq 'all') { @('arm64', 'x86') } else { @($Architecture) }
$profile = if ($Debug) { 'debug' } else { 'release' }
Push-Location $root
try {
    foreach ($arch in $arches) {
        $target = $targets[$arch]
        & $rustup target add $target
        if ($LASTEXITCODE -ne 0) { throw "Could not install $target." }
        $args = @('build', '--locked', '--bin', 'see_my_mac', '--target', $target)
        if (-not $Debug) { $args += '--release' }
        & $cargo @args
        if ($LASTEXITCODE -ne 0) { throw "Build failed for $target." }
        $binary = Join-Path $metadata.target_directory "$target\$profile\see_my_mac.exe"
        $suffix = if ($Debug) { '-debug' } else { '' }
        & $iscc "/DAppVersion=$version" "/DAppArch=$arch" "/DAppBinary=$binary" "/DBuildSuffix=$suffix" (Join-Path $root 'packaging\windows\seemymac.iss')
        if ($LASTEXITCODE -ne 0) { throw "Installer creation failed for $arch." }
    }
} finally { Pop-Location }
