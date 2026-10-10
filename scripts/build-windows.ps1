param([switch]$SkipInstaller)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)

cargo build --locked --release --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw 'Rust build failed' }

# Replace generated packages so stale files cannot ship.
if (Test-Path dist) { Remove-Item dist -Recurse -Force }
New-Item -ItemType Directory -Force dist | Out-Null

Copy-Item target\x86_64-pc-windows-msvc\release\hindsight.exe dist\
Copy-Item README.md, LICENSE dist\

if (!$SkipInstaller) {
    # Locate NSIS via PATH, standard installation directories, or Chocolatey.
    $nsis = Get-Command makensis.exe -ErrorAction SilentlyContinue

    if ($nsis) {
        $compiler = $nsis.Source
    } else {
        $candidates = @(
            "${env:ProgramFiles(x86)}\NSIS\makensis.exe",
            "$env:ProgramFiles\NSIS\makensis.exe",
            "$env:ChocolateyInstall\bin\makensis.exe",
            "$env:ChocolateyInstall\lib\nsis\tools\makensis.exe"
        )

        $compiler = $candidates |
            Where-Object { Test-Path $_ -PathType Leaf } |
            Select-Object -First 1

        if (!$compiler -and $env:ChocolateyInstall) {
            $chocoNsis = Join-Path $env:ChocolateyInstall 'lib\nsis'
            if (Test-Path $chocoNsis) {
                $compiler = Get-ChildItem $chocoNsis -Filter makensis.exe -File -Recurse -ErrorAction SilentlyContinue |
                    Select-Object -First 1 -ExpandProperty FullName
            }
        }

        if (!$compiler) {
            throw 'NSIS makensis.exe not found. Install NSIS 3 or pass -SkipInstaller.'
        }
    }

    Write-Host "Using NSIS compiler: $compiler"

    & $compiler /WX installer\hindsight.nsi
    if ($LASTEXITCODE -ne 0) { throw 'Installer compilation failed' }
}

Write-Host 'Built the Windows x64 application in dist.'

Get-ChildItem dist -File -Recurse |
    Where-Object { $_.Name -ne 'SHA256SUMS.txt' } |
    Sort-Object FullName |
    ForEach-Object {
        $relative = [IO.Path]::GetRelativePath((Join-Path $PWD 'dist'), $_.FullName).Replace('\', '/')
        "{0}  {1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $relative
    } | Set-Content dist/SHA256SUMS.txt -Encoding utf8
