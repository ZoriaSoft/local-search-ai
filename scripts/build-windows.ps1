$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$previousFlags = $env:CARGO_ENCODED_RUSTFLAGS
$flags = @()
if ($previousFlags) {
    $flags = @($previousFlags.Split([char]31))
} elseif ($env:RUSTFLAGS) {
    $flags = @($env:RUSTFLAGS -split '\s+' | Where-Object { $_ })
}

# Cargo does not expand environment variables inside rustflags in config.toml.
# Remap both Windows path spellings, including dependencies outside the checkout.
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }
$roots = @(
    @{ Path = $env:USERPROFILE; Target = "/build/home" },
    @{ Path = [IO.Path]::GetFullPath($cargoHome); Target = "/build/cargo" },
    @{ Path = $root; Target = "/build/source" }
)
foreach ($entry in $roots) {
    foreach ($path in @($entry.Path, $entry.Path.Replace('\', '/')) | Select-Object -Unique) {
        $flags += "--remap-path-prefix=$path=$($entry.Target)"
    }
}

Push-Location $root
try {
    # onnxruntime.dll needs the Visual C++ runtime (MSVCP140, MSVCP140_1,
    # VCRUNTIME140, VCRUNTIME140_1). A clean Windows has none of them, and the
    # target PCs are often offline, so ship them next to zoria-arsiv.exe
    # (app-local deployment; Windows looks in the exe folder first).
    $vcrt = Join-Path $root "resources\vcrt"
    New-Item -ItemType Directory -Force -Path $vcrt | Out-Null
    $vsRoots = @("${env:ProgramFiles(x86)}\Microsoft Visual Studio", "$env:ProgramFiles\Microsoft Visual Studio")
    $crtDir = Get-ChildItem $vsRoots -Recurse -Directory -Filter "Microsoft.VC14*.CRT" -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\Redist\\MSVC\\[0-9.]+\\x64\\' } |
        Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $crtDir) { throw "Visual C++ x64 redist folder (Microsoft.VC14x.CRT) not found." }
    foreach ($dll in @("msvcp140.dll", "msvcp140_1.dll", "vcruntime140.dll", "vcruntime140_1.dll")) {
        Copy-Item -Force (Join-Path $crtDir.FullName $dll) $vcrt
    }
    Write-Output "Bundled Visual C++ runtime from $($crtDir.FullName)"

    $env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]31
    & npm.cmd run tauri -- build
    if ($LASTEXITCODE -ne 0) { throw "Windows release build failed (exit $LASTEXITCODE)." }

    $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "target" }
    if (-not [IO.Path]::IsPathRooted($targetDir)) { $targetDir = Join-Path $root $targetDir }
    $binary = Join-Path $targetDir "release\zoria-arsiv.exe"
    $bytes = [IO.File]::ReadAllBytes($binary)
    foreach ($text in @([Text.Encoding]::ASCII.GetString($bytes), [Text.Encoding]::Unicode.GetString($bytes))) {
        if ($text -match '(?i)[a-z]:[\\/]+Users[\\/]') {
            throw "Privacy check failed: the release executable contains a Windows user-profile path."
        }
    }
    Write-Output "Release privacy check passed: no Windows user-profile paths."
} finally {
    $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags
    Pop-Location
}
