#!/usr/bin/env python3
"""Post-process samply Firefox profiler JSON so frames show demangled Rust names.

samply on macOS often leaves stringArray entries as ``0xOFFSET`` when DWARF
resolution fails or LTO/strip hid symbols. This script:

1. Loads the profile JSON (optionally gzipped).
2. Builds a symbol table from ``nm -n`` on the profiling binary (or binaries).
3. Replaces ``0x...`` strings that match text-section offsets with demangled
   function names (via ``rustfilt`` if available).
4. Writes a new ``*.named.json.gz`` next to the input (or ``-o`` path).

Usage:
  python3 bench/symbolicate_profile.py profile.json.gz \\
      --bin target/profiling/kcptun-server \\
      --bin target/profiling/kcptun-client

  # Also accept unstripped release:
  python3 bench/symbolicate_profile.py p.json.gz --bin target/release/kcptun-server
"""

from __future__ import annotations

import argparse
import gzip
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Dict, List, Optional, Sequence, Tuple

HEX_RE = re.compile(r"^0x([0-9a-fA-F]+)$")
# Mach-O / ELF typical preferred load address for aarch64 executables
DEFAULT_BASE = 0x100000000


def open_json(path: Path):
    if path.suffix == ".gz" or str(path).endswith(".json.gz"):
        with gzip.open(path, "rt", encoding="utf-8") as f:
            return json.load(f)
    with path.open("rt", encoding="utf-8") as f:
        return json.load(f)


def write_json_gz(path: Path, data) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with gzip.open(path, "wt", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, separators=(",", ":"))


def rustfilt_batch(names: Sequence[str]) -> List[str]:
    if not names:
        return []
    if not shutil.which("rustfilt"):
        return list(names)
    try:
        proc = subprocess.run(
            ["rustfilt"],
            input="\n".join(names) + "\n",
            text=True,
            capture_output=True,
            check=False,
        )
        if proc.returncode != 0:
            return list(names)
        out = proc.stdout.splitlines()
        # rustfilt may drop empty lines; pad
        if len(out) < len(names):
            out = out + list(names[len(out) :])
        return out[: len(names)]
    except OSError:
        return list(names)


def load_nm_symbols(binary: Path) -> List[Tuple[int, str]]:
    """Return sorted (addr, name) for text-ish symbols from nm -n."""
    if not binary.is_file():
        raise FileNotFoundError(f"binary not found: {binary}")
    proc = subprocess.run(
        ["nm", "-n", str(binary)],
        capture_output=True,
        text=True,
        errors="replace",
        check=False,
    )
    if proc.returncode != 0 and not proc.stdout:
        raise RuntimeError(f"nm failed on {binary}: {proc.stderr[:200]}")
    syms: List[Tuple[int, str]] = []
    for line in proc.stdout.splitlines():
        parts = line.split()
        if len(parts) < 3:
            continue
        # formats: "ADDR T name" or "ADDR t name"
        typ = parts[1]
        if typ not in ("t", "T", "w", "W", "s", "S"):
            continue
        try:
            addr = int(parts[0], 16)
        except ValueError:
            continue
        name = " ".join(parts[2:])
        if name in ("__mh_execute_header",):
            continue
        syms.append((addr, name))
    syms.sort(key=lambda x: x[0])
    return syms


def resolve_offset(
    offset: int, tables: Sequence[List[Tuple[int, str]]], base: int
) -> Optional[str]:
    """Map profile 0xOFFSET (relative to preferred base) or absolute addr to symbol."""
    candidates = [offset]
    if offset < base:
        candidates.append(base + offset)
    for addr in candidates:
        for syms in tables:
            if not syms:
                continue
            # binary search last symbol <= addr
            lo, hi = 0, len(syms) - 1
            best = None
            while lo <= hi:
                mid = (lo + hi) // 2
                if syms[mid][0] <= addr:
                    best = syms[mid]
                    lo = mid + 1
                else:
                    hi = mid - 1
            if best is None:
                continue
            # reject if too far past symbol end (heuristic 256 KiB)
            if addr - best[0] > 256 * 1024:
                continue
            return best[1]
    return None


def short_display_name(demangled: str) -> str:
    """Keep readable Rust names; strip hash suffixes and trim extreme length."""
    s = demangled
    # drop ::hHASH at end of monomorphized items when present as separate suffix
    s = re.sub(r"::h[0-9a-f]{16}$", "", s)
    # common noise: full core:: paths still useful; cap length
    if len(s) > 180:
        s = s[:177] + "..."
    return s


def symbolicate(
    profile: dict,
    binaries: Sequence[Path],
    base: int,
) -> Tuple[dict, dict]:
    tables = [load_nm_symbols(b) for b in binaries]
    stats = {
        "strings_total": 0,
        "hex_seen": 0,
        "hex_resolved": 0,
        "mangled_demangled": 0,
        "binaries": [str(b) for b in binaries],
        "symbols_loaded": sum(len(t) for t in tables),
    }

    # Collect all unique strings that need work
    hex_to_raw: Dict[str, str] = {}
    mangled: List[str] = []

    for thread in profile.get("threads") or []:
        sa = thread.get("stringArray") or []
        stats["strings_total"] += len(sa)
        for s in sa:
            if not isinstance(s, str):
                continue
            m = HEX_RE.match(s)
            if m:
                stats["hex_seen"] += 1
                if s not in hex_to_raw:
                    off = int(m.group(1), 16)
                    raw = resolve_offset(off, tables, base)
                    if raw:
                        hex_to_raw[s] = raw
                        stats["hex_resolved"] += 1
            elif s.startswith("_ZN") or s.startswith("__ZN") or s.startswith("_RNv"):
                mangled.append(s)

    # Demangle unique names
    unique_mangled = sorted(set(mangled) | set(hex_to_raw.values()))
    demangled_list = rustfilt_batch(unique_mangled)
    demap = {
        unique_mangled[i]: short_display_name(demangled_list[i])
        for i in range(len(unique_mangled))
    }
    stats["mangled_demangled"] = sum(
        1 for k, v in demap.items() if v != k and not HEX_RE.match(k)
    )

    # Apply to all threads' stringArray
    for thread in profile.get("threads") or []:
        sa = thread.get("stringArray")
        if not sa:
            continue
        new_sa = []
        for s in sa:
            if not isinstance(s, str):
                new_sa.append(s)
                continue
            if s in hex_to_raw:
                raw = hex_to_raw[s]
                new_sa.append(demap.get(raw, short_display_name(raw)))
            elif s in demap:
                new_sa.append(demap[s])
            else:
                new_sa.append(s)
        thread["stringArray"] = new_sa

    # Mark as symbolicated if field exists
    meta = profile.get("meta")
    if isinstance(meta, dict):
        meta["symbolicated"] = True
        meta.setdefault("product", meta.get("product") or "kcptun-rs")
        extras = meta.setdefault("configuration", {})
        if not isinstance(extras, dict):
            extras = {}
            meta["configuration"] = extras
        extras["kcptun_symbolicate"] = {
            "binaries": stats["binaries"],
            "hex_resolved": stats["hex_resolved"],
            "hex_seen": stats["hex_seen"],
        }

    return profile, stats


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("profile", type=Path, help="Input .json or .json.gz from samply")
    ap.add_argument(
        "--bin",
        action="append",
        dest="bins",
        type=Path,
        default=[],
        help="Binary with symbols (repeatable). Default: target/profiling/kcptun-{server,client}",
    )
    ap.add_argument(
        "-o",
        "--output",
        type=Path,
        default=None,
        help="Output path (default: <input stem>.named.json.gz)",
    )
    ap.add_argument(
        "--base",
        type=lambda x: int(x, 0),
        default=DEFAULT_BASE,
        help="Preferred load address (default 0x100000000 for aarch64 Mach-O)",
    )
    ap.add_argument(
        "--in-place",
        action="store_true",
        help="Overwrite input path (still writes gzip if input was gz)",
    )
    args = ap.parse_args()

    bins = args.bins
    if not bins:
        root = Path.cwd()
        for rel in (
            "target/profiling/kcptun-server",
            "target/profiling/kcptun-client",
            "target/release/kcptun-server",
            "target/release/kcptun-client",
        ):
            p = root / rel
            if p.is_file():
                bins.append(p)
        # de-dup preserve order
        seen = set()
        bins = [b for b in bins if not (str(b) in seen or seen.add(str(b)))]

    if not bins:
        print("error: no --bin and no target/profiling or target/release binaries", file=sys.stderr)
        return 2

    try:
        profile = open_json(args.profile)
    except Exception as e:
        print(f"error: load {args.profile}: {e}", file=sys.stderr)
        return 1

    try:
        profile, stats = symbolicate(profile, bins, args.base)
    except Exception as e:
        print(f"error: symbolicate: {e}", file=sys.stderr)
        return 1

    if args.in_place:
        out = args.profile
        if not str(out).endswith(".gz"):
            out = out.with_suffix(out.suffix + ".gz") if out.suffix else Path(str(out) + ".gz")
    elif args.output:
        out = args.output
    else:
        # foo.json.gz -> foo.named.json.gz
        name = args.profile.name
        if name.endswith(".json.gz"):
            out = args.profile.with_name(name[: -len(".json.gz")] + ".named.json.gz")
        elif name.endswith(".json"):
            out = args.profile.with_name(name[: -len(".json")] + ".named.json.gz")
        else:
            out = Path(str(args.profile) + ".named.json.gz")

    write_json_gz(out, profile)
    print(
        f"wrote {out}\n"
        f"  symbols_loaded={stats['symbols_loaded']} binaries={len(stats['binaries'])}\n"
        f"  hex_seen={stats['hex_seen']} hex_resolved={stats['hex_resolved']}\n"
        f"  open: samply load {out}"
    )
    if stats["hex_seen"] and stats["hex_resolved"] == 0:
        print(
            "warning: no hex offsets resolved — rebuild with:\n"
            "  cargo build --profile profiling -p kcptun-server -p kcptun-client\n"
            "  # and pass those binaries via --bin",
            file=sys.stderr,
        )
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())
