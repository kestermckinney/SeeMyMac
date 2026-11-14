# Builds the Windows installer on a GitLab-hosted Windows runner (saas-windows-medium-amd64).
# The image has Visual Studio and Chocolatey but not everything tools\build-installers.ps1 needs,
# so this installs the rest first, then runs that same script. Output:
#   target\installers\SeeMyMac-<version>-setup.exe

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$root = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent

function Invoke-Checked([string]$what, [scriptblock]$block) {
    & $block
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit $LASTEXITCODE)." }
}

# The tag must name the version being built.
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "([0-9.]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if ($env:CI_COMMIT_TAG -and $env:CI_COMMIT_TAG -ne "v$version") { throw "Tag $env:CI_COMMIT_TAG does not match Cargo.toml version $version." }

# Inno Setup, NASM (aws-lc-sys on x86) and LLVM/Clang.
Invoke-Checked 'Chocolatey' { choco install -y --no-progress innosetup nasm llvm }

# The C++ tools for the ARM64 and x86 targets.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw 'Visual Studio with the C++ tools was not found on this runner.' }
$arm64 = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.ARM64 -property installationPath
if (-not $arm64) {
    Write-Host "Adding the ARM64 C++ tools to $vs"
    $installer = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\setup.exe'
    $p = Start-Process -FilePath $installer -Wait -PassThru -ArgumentList @('modify', '--installPath', "`"$vs`"", '--add', 'Microsoft.VisualStudio.Component.VC.Tools.ARM64', '--quiet', '--norestart', '--nocache')
    if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) { throw "Visual Studio installer failed (exit $($p.ExitCode))." }
}

# Rust, where build-installers.ps1 expects it.
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path $cargo)) {
    $init = Join-Path $env:TEMP 'rustup-init.exe'
    Invoke-WebRequest -UseBasicParsing -Uri 'https://win.rustup.rs/x86_64' -OutFile $init
    Invoke-Checked 'rustup-init' { & $init -y --profile minimal --default-toolchain stable }
}
$env:Path = "$(Join-Path $env:USERPROFILE '.cargo\bin');$env:Path"

& (Join-Path $root 'tools\build-installers.ps1')
$exe = Join-Path $root "target\installers\SeeMyMac-$version-setup.exe"
if (-not (Test-Path $exe)) { throw "Missing $exe." }
Get-Item $exe | Format-List Name, Length
