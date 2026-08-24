#!/usr/bin/env bash
#
# Compile a TinyC program all the way to a running executable, on Linux.
#
#   ./scripts/build.sh examples/hello.tc
#
# tinyc itself only emits assembly; this script takes it the rest of the way
# with nasm and the system C compiler:
#
#   tinyc  source.tc  -> source.asm
#   nasm   source.asm -> source.o     (-f elf64)
#   cc     source.o   -> source       (linked against the C library for printf)
#
# The counterpart of scripts/build.ps1, which does the same on Windows.

set -euo pipefail

usage() {
    echo "usage: $0 [--out-dir DIR] [--no-run] SOURCE.tc" >&2
    exit 2
}

out_dir="out"
run=1
source_file=""

while [ $# -gt 0 ]; do
    case "$1" in
        --out-dir) out_dir="${2:?--out-dir needs a directory}"; shift 2 ;;
        --no-run)  run=0; shift ;;
        -h|--help) usage ;;
        -*)        echo "unknown option: $1" >&2; usage ;;
        *)         [ -n "$source_file" ] && usage; source_file="$1"; shift ;;
    esac
done
[ -n "$source_file" ] || usage

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
name="$(basename "$source_file" .tc)"
out="$root/$out_dir"
asm="$out/$name.asm"
obj="$out/$name.o"
exe="$out/$name"

mkdir -p "$out"

# 1. TinyC -> assembly. `--target` is left out: it defaults to this machine.
echo "==> tinyc $source_file"
cargo run --quiet --manifest-path "$root/Cargo.toml" -- "$source_file" -o "$asm"

# 2. Assemble. NASM's elf64 output is an ELF object, which cc will link.
command -v nasm >/dev/null || { echo "nasm not found (apt install nasm)" >&2; exit 1; }
echo "==> nasm"
nasm -f elf64 -o "$obj" "$asm"

# 3. Link with the C compiler, which is what knows where the C library is and
#    which startup object calls main.
#
#    -no-pie: a position-independent executable reaches every symbol through
#    the GOT or the PLT, and assembly that names them outright does not. Most
#    distributions link PIE by default, so this flag is not optional.
#
#    -lpthread: `pthread_getattr_np` is how the prologue's stack check finds out
#    where the stack ends. Since glibc 2.34 it is in libc proper and this is a
#    no-op; on anything older it is where the symbol lives.
cc=""
for candidate in cc gcc clang; do
    if command -v "$candidate" >/dev/null; then cc="$candidate"; break; fi
done
[ -n "$cc" ] || { echo "no C compiler found (apt install build-essential)" >&2; exit 1; }
echo "==> $cc"
"$cc" -no-pie "$obj" -o "$exe" -lpthread

echo "==> built $exe"
if [ "$run" -eq 1 ]; then
    echo "==> running"
    set +e
    "$exe"
    status=$?
    set -e
    echo "==> exit code $status"
fi
