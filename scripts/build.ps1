# Compile a TinyC program all the way to a running executable.
#
#   .\scripts\build.ps1 examples\hello.tc
#
# tinyc itself only emits assembly; this script takes it the rest of the way with
# nasm and the Microsoft linker:
#
#   tinyc  source.tc  -> source.asm
#   nasm   source.asm -> source.obj   (-f win64, a COFF object)
#   link   source.obj -> source.exe   (linked against the C runtime for printf)

[CmdletBinding()]
param(
    # TinyC source file to compile.
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Source,

    # Directory for the generated .asm, .obj and .exe.
    [string]$OutDir = "out",

    # Only build; do not run the resulting executable.
    [switch]$NoRun
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$name = [System.IO.Path]::GetFileNameWithoutExtension($Source)
$outPath = Join-Path $root $OutDir
$asm = Join-Path $outPath "$name.asm"
$obj = Join-Path $outPath "$name.obj"
$exe = Join-Path $outPath "$name.exe"

New-Item -ItemType Directory -Force -Path $outPath | Out-Null

# 1. TinyC -> assembly.
Write-Host "==> tinyc $Source"
& cargo run --quiet --manifest-path (Join-Path $root "Cargo.toml") -- $Source -o $asm
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# 2. Locate nasm. `winget install nasm` does not put it on PATH, so fall back to
#    the places it normally lands.
$nasm = (Get-Command nasm.exe -ErrorAction SilentlyContinue).Source
if (-not $nasm) {
    $candidates = @(
        "$env:LOCALAPPDATA\bin\NASM\nasm.exe",
        "$env:ProgramFiles\NASM\nasm.exe",
        "${env:ProgramFiles(x86)}\NASM\nasm.exe"
    )
    $nasm = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $nasm) {
    throw "nasm.exe not found. Install it with 'winget install nasm', or put it on PATH."
}

# 3. Assemble. NASM's win64 output is a COFF object, exactly what link.exe wants.
Write-Host "==> nasm"
& $nasm -f win64 -o $obj $asm
if ($LASTEXITCODE -ne 0) { throw "assembling failed" }

# 4. Locate the Microsoft linker (nasm does not ship one).
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) {
    throw "vswhere.exe not found: Visual Studio (or the Build Tools) does not appear to be installed."
}
$vsPath = & $vswhere -latest -products * -property installationPath
$vcvars = Join-Path $vsPath "VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $vcvars)) {
    throw "vcvars64.bat not found under $vsPath. Install the 'Desktop development with C++' workload."
}

# 5. Link inside a developer command prompt, which sets the LIB paths link needs.
#    - msvcrt.lib                    : the C runtime (printf lives here)
#    - legacy_stdio_definitions.lib  : exports printf as a real symbol rather
#                                      than the inline function the UCRT headers
#                                      normally provide
Write-Host "==> link"
$commands = @(
    "call `"$vcvars`" >nul 2>&1",
    "link /nologo /subsystem:console /entry:mainCRTStartup /out:`"$exe`" `"$obj`" msvcrt.lib legacy_stdio_definitions.lib"
) -join " && "

& cmd.exe /c $commands
if ($LASTEXITCODE -ne 0) { throw "linking failed" }

Write-Host "==> built $exe"
if (-not $NoRun) {
    Write-Host "==> running"
    & $exe
    Write-Host "==> exit code $LASTEXITCODE"
}
