import re, statistics, sys

files = {
    'baseline':         'bench/results_cl_baseline.txt',
    'opt2_alone':       'bench/results_cl_opt2_no_readop.txt',
    'opt2+3 (1st)':     'bench/results_cl_opt3_prefetch4.txt',
    'opt2+3 (2nd)':     'bench/results_cl_combined_opt2_opt3.txt',
    'opt2+3 (3rd)':     'bench/results_cl_reverify_opt2_3.txt',
}

def parse(fn):
    p50, p90, p99, p999, rps = [], [], [], [], []
    with open(fn) as f:
        for line in f:
            if not line.startswith('RESULT'):
                continue
            m = dict(re.findall(r'(\w+)=(\S+)', line))
            p50.append(float(m['p50_us']))
            p90.append(float(m['p90_us']))
            p99.append(float(m['p99_us']))
            p999.append(float(m['p999_us']))
            rps.append(float(m['actual_rps']))
    return p50, p90, p99, p999, rps

header = f"{'Test':<20} {'P50':>8} {'P90':>8} {'P99':>8} {'P999':>8} {'RPS':>8}  | {'P999 min':>8} {'P999 max':>8} {'range':>6}"
print(header)
print('-' * len(header))
for label, fn in files.items():
    p50, p90, p99, p999, rps = parse(fn)
    med = statistics.median
    rng = f"{max(p999)/min(p999):.1f}x"
    print(f"{label:<20} {med(p50):>8.0f} {med(p90):>8.0f} {med(p99):>8.0f} {med(p999):>8.0f} {med(rps):>8.0f}  | {min(p999):>8.0f} {max(p999):>8.0f} {rng:>6}")

print()
print("=== P999 sorted values (all 10 runs) ===")
for label, fn in files.items():
    _, _, _, p999, _ = parse(fn)
    p999_sorted = sorted(p999)
    print(f"{label:<20}: {[int(x) for x in p999_sorted]}")

print()
print("=== P99 sorted values (all 10 runs) ===")
for label, fn in files.items():
    _, _, p99, _, _ = parse(fn)
    p99_sorted = sorted(p99)
    print(f"{label:<20}: {[int(x) for x in p99_sorted]}")

print()
print("=== RPS sorted values (all 10 runs) ===")
for label, fn in files.items():
    _, _, _, _, rps = parse(fn)
    rps_sorted = sorted(rps)
    print(f"{label:<20}: {[int(x) for x in rps_sorted]}")
