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
