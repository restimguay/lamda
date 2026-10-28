#!/usr/bin/env bash
# Builds each benchmark with lamda and with rustc, checks the outputs match,
# and reports the best wall-clock time of several runs.
set -euo pipefail
cd "$(dirname "$0")"
RUNS=${RUNS:-7}
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

cargo build --release --quiet --manifest-path ../Cargo.toml
[ -f data/log.txt ] || python3 gen_log.py >/dev/null
LAMDA=../target/release/lamda

best() {
    python3 - "$RUNS" "$@" <<'PY'
import subprocess, sys, time
runs, cmd = int(sys.argv[1]), sys.argv[2:]
times = []
for _ in range(runs):
    t = time.perf_counter()
    subprocess.run(cmd, stdout=subprocess.DEVNULL, check=True)
    times.append(time.perf_counter() - t)
print(f"{min(times) * 1000:8.1f} ms")
PY
}

printf "%-8s %12s %12s %20s\n" bench lamda rust "rust (native cpu)"
for name in ${BENCHES:-fib sieve sieve8 gather closure mandel wc strfind strsum alloc recount resum realt reword}; do
    $LAMDA build "$name.lm" -o "$OUT/$name-lm"
    if [ -f "$name.rs" ]; then
        rustc -C opt-level=3 -C panic=abort -o "$OUT/$name-rs" "$name.rs"
        rustc -C opt-level=3 -C panic=abort -C target-cpu=native -o "$OUT/$name-rsn" "$name.rs"
    else
        # Benchmarks that need crates live in rust-regex/ as Cargo binaries.
        cargo build --release --quiet --manifest-path rust-regex/Cargo.toml --target-dir "$OUT/cargo" --bin "$name"
        RUSTFLAGS="-C target-cpu=native" cargo build --release --quiet --manifest-path rust-regex/Cargo.toml \
            --target-dir "$OUT/cargo-native" --bin "$name"
        cp "$OUT/cargo/release/$name" "$OUT/$name-rs"
        cp "$OUT/cargo-native/release/$name" "$OUT/$name-rsn"
    fi
    a=$("$OUT/$name-lm"); b=$("$OUT/$name-rs")
    if [ "$a" != "$b" ]; then echo "$name: output mismatch: lamda=$a rust=$b" >&2; exit 1; fi
    printf "%-8s %12s %12s %20s\n" "$name" "$(best "$OUT/$name-lm")" "$(best "$OUT/$name-rs")" "$(best "$OUT/$name-rsn")"
done
