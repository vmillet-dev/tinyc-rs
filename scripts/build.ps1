# Compile a TinyC program all the way to a running executable.
#
#   .\scripts\build.ps1 examples\hello.tc
#
# tinyc itself only emits assembly; this script takes it the rest of the way by
# calling the Microsoft assembler and linker from a Visual Studio installation:
#
#   tinyc  source.tc -> source.asm
#   ml64   source.asm -> source.obj
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

# 2. Locate the Visual Studio build tools (ml64.exe and link.exe live there).
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) {
    throw "vswhere.exe not found: Visual Studio (or the Build Tools) does not appear to be installed."
}
$vsPath = & $vswhere -latest -products * -property installationPath
$vcvars = Join-Path $vsPath "VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $vcvars)) {
    throw "vcvars64.bat not found under $vsPath. Install the 'Desktop development with C++' workload."
}

# 3. Assemble and link inside a developer command prompt, which sets the INCLUDE
#    and LIB paths that ml64 and link need.
#    - msvcrt.lib                    : the C runtime (printf lives here)
#    - legacy_stdio_definitions.lib  : exports printf as a real symbol rather
#                                      than the inline function the UCRT headers
#                                      normally provide
Write-Host "==> ml64 + link"
$commands = @(
    "call `"$vcvars`" >nul 2>&1",
    "ml64 /nologo /c /Fo `"$obj`" `"$asm`"",
    "link /nologo /subsystem:console /entry:mainCRTStartup /out:`"$exe`" `"$obj`" msvcrt.lib legacy_stdio_definitions.lib"
) -join " && "

& cmd.exe /c $commands
if ($LASTEXITCODE -ne 0) { throw "assembling or linking failed" }

Write-Host "==> built $exe"
if (-not $NoRun) {
    Write-Host "==> running"
    & $exe
    Write-Host "==> exit code $LASTEXITCODE"
}
