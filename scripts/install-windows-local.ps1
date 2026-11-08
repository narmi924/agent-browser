# Builds this checkout and replaces the global agent-browser executable with it.
# Return to the published package with: npm install -g agent-browser

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

$vcvars = @('C:\Program Files\Microsoft Visual Studio', 'C:\Program Files (x86)\Microsoft Visual Studio') |
    Where-Object { Test-Path $_ } |
    ForEach-Object { Get-ChildItem $_ -Directory | ForEach-Object { Get-ChildItem $_.FullName -Directory } } |
    ForEach-Object { Join-Path $_.FullName 'VC\Auxiliary\Build\vcvars64.bat' } |
    Where-Object { Test-Path $_ } |
    Select-Object -First 1
if (-not $vcvars) { throw 'Visual Studio C++ build tools were not found.' }

$manifest = Join-Path $repo 'cli\Cargo.toml'
& cmd.exe /c "call `"$vcvars`" >nul 2>&1 && cargo build --release --locked --manifest-path `"$manifest`""
if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
$binary = Join-Path $repo 'cli\target\release\agent-browser.exe'

$package = Join-Path (& npm root -g) 'agent-browser'
if (-not (Test-Path $package)) { throw 'Install the published package first: npm install -g agent-browser' }
$target = Join-Path $package 'bin\agent-browser-win32-x64.exe'
$previous = "$target.previous"

# A running daemon keeps the file open; Windows still allows it to be renamed.
Remove-Item $previous -Force -ErrorAction SilentlyContinue
if (Test-Path $target) { Move-Item $target $previous -Force }
Copy-Item $binary $target -Force

if ((Get-FileHash $target).Hash -ne (Get-FileHash $binary).Hash) {
    throw "The installed binary at $target does not match this build."
}
Write-Output "Installed $target"
