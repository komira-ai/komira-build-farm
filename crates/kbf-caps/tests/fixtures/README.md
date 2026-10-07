# Capability fixtures

One file per node class, in the exact format the OS prints:

| File | Node class | Source text |
|---|---|---|
| `broadwell_e5_2699_v4.cpuinfo` | Intel Xeon E5-2699 v4 (Broadwell-EP) | Linux `/proc/cpuinfo` |
| `skylake_sp_platinum_8180.cpuinfo` | Intel Xeon Platinum 8180 (Skylake-SP) | Linux `/proc/cpuinfo` |
| `ampere_altra_max_m128_30.cpuinfo` | Ampere Altra Max M128-30 (Neoverse N1) | Linux `/proc/cpuinfo` |
| `apple_m3_ultra.sysctl` | Apple M3 Ultra | macOS `sysctl hw.optional` |

The cpuinfo files hold two processor blocks each, trimmed from a full listing; the
parser keeps the features common to every block.

These are reconstructed from each CPU model's published feature set, not captured
from one of our nodes. When a node of a class first registers, its real output
replaces the file here, and any test that then changes is a finding, not a fixture
to edit.
