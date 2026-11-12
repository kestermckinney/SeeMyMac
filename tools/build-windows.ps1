param(
    [switch]$Release,
    [switch]$Test,
    [ValidateSet('x86', 'arm64')]
    [string]$Architecture,
    [switch]$Run
)

$ErrorActionPreference = 'Stop'
Push-Location (Split-Path $PSScriptRoot -Parent)
try {
    if (-not $env:GPUI_FXC_PATH) {
        $sdkBin = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
        $compiler = Get-ChildItem -Path "$sdkBin\*\x64\fxc.exe" -ErrorAction SilentlyContinue |
            Sort-Object { [version]$_.Directory.Parent.Name } -Descending |
            Select-Object -First 1
        if (-not $compiler) {
            throw 'Install the Windows SDK with the Visual Studio Desktop development with C++ workload, or set GPUI_FXC_PATH to fxc.exe.'
        }
        $env:GPUI_FXC_PATH = $compiler.FullName
    }
    $cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
    if (-not $cargo) {
        $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    }
    if (-not (Test-Path -LiteralPath $cargo)) {
        throw 'Install the stable Rust MSVC toolchain using Rustup.'
    }
    $llvm = Join-Path $env:ProgramFiles 'LLVM\bin'
    if (Test-Path -LiteralPath $llvm) {
        $env:Path = "$llvm;$env:Path"
        if (-not $env:LIBCLANG_PATH) { $env:LIBCLANG_PATH = $llvm }
    }
    $profileArgs = @()
    if ($Architecture) {
        $targets = @{ x86 = 'i686-pc-windows-msvc'; arm64 = 'aarch64-pc-windows-msvc' }
        $profileArgs += @('--target', $targets[$Architecture])
    }
    if ($Release) { $profileArgs += '--release' }
    & $cargo build --locked @profileArgs
    if ($LASTEXITCODE -ne 0) { throw "Build failed (exit $LASTEXITCODE)." }
    if ($Run) {
        & $cargo run --locked --bin see_my_mac @profileArgs
        if ($LASTEXITCODE -ne 0) { throw "Run failed (exit $LASTEXITCODE)." }
    }
    if ($Test) {
        & $cargo test --locked --all-targets @profileArgs
        if ($LASTEXITCODE -ne 0) { throw "Tests failed (exit $LASTEXITCODE)." }
    }
} finally {
    Pop-Location
}
