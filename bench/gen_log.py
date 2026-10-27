#!/usr/bin/env python3
"""Writes data/log.txt: ~110 MB of deterministic, log-like text for the string benchmarks."""
import pathlib

LINES = 1_500_000
LEVELS = ["INFO"] * 8 + ["WARN", "ERROR"]
STATUS = [200] * 12 + [201, 204, 301, 404, 404, 500]
PATHS = ["/api/items/", "/api/users/", "/login", "/static/app.js", "/api/orders/"]


def main():
    out = pathlib.Path(__file__).resolve().parent / "data" / "log.txt"
    out.parent.mkdir(exist_ok=True)
    x = 12345
    lines = []
    for i in range(LINES):
        x = (x * 6364136223846793005 + 1442695040888963407) % 2**64
        r = x >> 16
        s = i % 86400
        path = PATHS[r % 5] + (str(r % 10000) if PATHS[r % 5].endswith("/") else "")
        lines.append(
            f"2026-10-27 {s // 3600:02}:{s // 60 % 60:02}:{s % 60:02} {LEVELS[r % 10]:<5} "
            f"user={r % 9973} path={path} status={STATUS[(r >> 8) % 18]} ms={(r >> 16) % 2000}\n"
        )
    out.write_text("".join(lines))
    print(out)


if __name__ == "__main__":
    main()
