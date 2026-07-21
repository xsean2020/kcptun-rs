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
