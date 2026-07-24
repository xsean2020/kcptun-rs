# bench/profiles

Generated flamegraph / samply artifacts live here. **Do not commit large binary or JSON profiles.**

## Regenerate

```bash
make release
bash bench/profile_flamegraph.sh all
```

Artifacts:

| File pattern | Scenario |
|--------------|----------|
| `L1-null-nocomp-*.json` | Bulk null + nocomp |
| `L2-aes-nocomp-*.json` | Bulk AES + nocomp |
| `L3-3des-nocomp-*.json` | Bulk 3des + nocomp |
| `L4-stress-*.json` | Multi-conn stress under sampler |

Interpretation notes (committed): `HOTSPOTS.md` (created after first real capture).

## Go-compatible pprof (protobuf)

CPU + heap/allocs profiles for `go tool pprof`.

```bash
make profiling-bins
make profile            # Rust → bench/profiles/rust-*.pb (+ -heap.pb, -allocs.pb)
make profile-go         # Go   → bench/profiles/go-*.pb.gz
```

Artifacts:

| File pattern             | Description                     |
|--------------------------|---------------------------------|
| `rust-*.pb`              | Rust CPU (Go pprof protobuf)    |
| `rust-*-heap.pb`         | Rust heap (inuse)               |
| `rust-*-allocs.pb`       | Rust cumulative allocs          |
| `go-*.pb.gz`             | Go kcptun CPU                   |

View:
```bash
go tool pprof -http=:0 bench/profiles/rust-server-aes-*.pb
go tool pprof -http=:0 bench/profiles/rust-server-aes-*-heap.pb
```
