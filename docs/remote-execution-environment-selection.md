# Remote-execution reference environment selection — issue #373

Bounded feasibility spike for the v0.4 milestone (#373): select one
compact, Docker-free remote-execution reference environment for
memory-constrained Mac/Linux machines. This document records measured
candidates, a ranking, and a single pinned recipe. It is not a backend
implementation and it runs no end-to-end protocol scenario.

Status: measurements were taken on branch `issue-373-agent` over base
revision `63905c6` (2026-11-23, JST); this document is committed on the
same branch.

#373 is marked blocked by #372. The environment selection groundwork
below does not depend on #372's open design decisions; every conclusion
that does is marked **provisional** inline.

## Tested configuration

| Field | Value |
| --- | --- |
| Host | Mac (arm64 / Apple Silicon), 6 cores, 8 GiB RAM |
| Host OS | macOS 26.7.1 (25G241) |
| Host memory state | `memory_pressure` free 30–49% across the runs; heavy compressor use — this host is at the intended low-memory boundary |
| Lima | 2.3.0 (`brew install lima`, 113 MB installed, no runtime dependency on QEMU) |
| QEMU | 11.1.3 (`brew install qemu`, ~800 MB with 9 dependencies — installed for comparison measurement only, not part of the recipe) |
| Guest image | `ubuntu-24.04-minimal-cloudimg-arm64.img`, release-20261105, `sha256:3dc8370d923978d4b610fa7964d1b36ba040eefb43198809ca5bc8ac51777b18` |
| Measurement tools | `footprint -p <pid>` (physical footprint), `ps rss`, `memory_pressure`, `vm_stat` wired/compressor ledgers, guest `free -m` |

## Host inventory

| Tool | State | Relevance |
| --- | --- | --- |
| `limactl` | absent → installed 2.3.0 via Homebrew for this spike | primary candidate |
| `qemu-system-aarch64` | absent → installed 11.1.3 via Homebrew for this spike | comparison candidate |
| OrbStack | installed and running; `docker` CLI answers (server 29.8.2) | inventory only — always-on resident daemons and a commercial licence for non-personal use make it ineligible as the reference environment (see Ranking, item 4); footprint recorded below as context |
| Docker Desktop | absent | per issue, Docker must not become a requirement |
| colima, nerdctl, vfkit, Tart, UTM, Parallels, VMware | absent | — |
| hypervisor.framework frontends | none besides Lima (vz) and OrbStack | — |

## Candidates

The issue bounds the set to a minimal lightweight Linux VM and a
genuinely suitable microVM-class option. On macOS the honest candidate
pair is Lima's two drivers, which run the same guest and differ only in
the VMM:

| Candidate | Availability evidence |
| --- | --- |
| Lima `vmType: vz` (Virtualization.framework) | runnable; measured below |
| Lima `vmType: qemu` (QEMU + `accel=hvf`) | runnable; measured below |
| Firecracker-class microVM on macOS | **not runnable.** Firecracker requires KVM (`/dev/kvm`), which exists only on Linux — its own getting-started document lists "Linux + KVM" as the prerequisite. Recorded as evidence; no Mac path is claimed. |
| Native macOS sandbox | explicitly out of scope per issue and brief; the v0.3 boundary is Linux bubblewrap containment |

## Measurements

Process-level figures are `phys_footprint` of the stated process; the
system-level series is `memory_pressure`/`vm_stat` (16 KiB pages). All
runs are sequential — at most one guest alive at a time.

### Headline table

| Candidate | Guest config | First start | Warm start | Idle host footprint | Active host footprint | Disk on host |
| --- | --- | --- | --- | --- | --- | --- |
| Lima vz | 1 CPU / 1 GiB / 10 GiB disk | 56 s (incl. image conversion + cloud-init) | 9.8 s | **98–100 MB** (single `limactl hostagent` process, VMM in-process) | 98 MB during a guest-internal 300 MB allocation | ~1.0 GB instance + 826 MB shared image cache |
| Lima vz | 1 CPU / 512 MiB | 32.8 s (cached image, first boot) | — | 96 MB | guest OOM-killed a 400 MB allocation — below the safe floor | ~1.0 GB |
| Lima qemu | 1 CPU / 1 GiB / 10 GiB disk | 32.7 s (first boot, cached qcow2) | 7.1 s | **~1.84 GB** (`qemu-system-aarch64` 1762 MB + `hostagent` 82 MB) | 1.85 GB during the same guest-internal 300 MB allocation | 642 MB instance |
| OrbStack (inventory, not a candidate) | shared Linux VM already running | — | — | ~384 MB resident (app 48 MB + `vmgr`/shared-VM 336 MB, peak 681 MB) | — | — |
| Firecracker | — | — | — | not runnable on macOS (requires Linux KVM) | — | — |

### Configured guest RAM vs actual host usage

The two drivers account guest memory in opposite ways:

- **vz** — configured RAM is a cap, not a reservation. A 1 GiB guest
  that has touched ~180 MB reads as a ~100 MB host footprint; a 512 MiB
  guest reads ~96 MB. Host-side per-process accounting
  (`phys_footprint`, `ps rss`) does **not** fully track guest-touched
  memory — part of it lands in system ledgers (wired, compressor) and
  in memory the kernel compresses elsewhere to make room. See the
  accounting caveats below.
- **qemu** — configured RAM is effectively resident: a 1 GiB guest
  costs ~1.76 GB in `qemu-system-aarch64` before the guest does
  anything (~700 MB of QEMU overhead on top), and grows to ~1.85 GB
  under a guest-internal 300 MB working set.

System-level series for the vz 1 GiB guest on this 8 GiB host
(pages × 16 KiB; compressor figures include unrelated system churn and
are upper bounds, not clean attribution):

| Point | `memory_pressure` free | wired pages | compressor-occupied pages | note |
| --- | --- | --- | --- | --- |
| A — all guests stopped | 49% | 117,760 | 131,010 | baseline |
| B — guest idle (guest reports ~180 MB used) | 41% | 121,805 | 167,723 | +63 MB wired, +574 MB compressor vs A |
| C — guest at ~460 MB used (300 MB alloc) | 38% | 120,523 | 181,438 | +789 MB compressor vs A |
| D — guest stopped again | 43% | 126,895 | 151,080 | all lima/qemu processes exited; compressor decays lazily |

Even under the worst-case reading (process footprint plus the full
observed compressor delta — ~98–100 MB + ~789 MB ≈ 0.89 GB), the vz
guest's marginal host cost stays roughly 2x below the qemu guest's
1.84 GB resident. On this host the dominant cost of a vz guest is that
it *pressures* the compressor, not that it *reserves* memory.

### Component breakdown (host-visible, vz candidate)

| Component | Where it runs | Measured cost |
| --- | --- | --- |
| VMM + guest (`limactl hostagent`, VZ framework in-process) | host | 98–100 MB footprint for the 1 GiB guest |
| usernet (`user-v2`, gVisor-class NAT) | inside `hostagent` | included above — no helper process, no sudo |
| SSH ControlMaster mux | host | ~0.7–1.4 MB |
| Guest agent (`lima-guestagent`) | inside guest | not host-resident; part of the guest's ~180 MB used |
| Coordinator (taisan Core), executor, transport helper, egress proxy | not part of this spike — **provisional** on #372's topology; nothing in the recipe requires them to be guest-resident | not measured |
| Runtime server / container daemon | none — `containerd` disabled in the recipe | 0 |

### v0.3 namespace-level containment in the guest

The namespace layer of the approved v0.3 profile (bubblewrap
user/mount/PID/network/IPC namespaces — `docs/worker-protocol.md`
selected profile, `docs/v0.3.md` OP-1) runs in the guest as an
unprivileged user after the recipe's one-time provisioning:

```
$ bwrap --unshare-user --unshare-pid --unshare-net --unshare-ipc \
    --new-session --die-with-parent --ro-bind /usr /usr \
    --symlink usr/bin /bin --symlink usr/sbin /sbin --symlink usr/lib /lib \
    --tmpfs /tmp --proc /proc --dev /dev \
    /bin/sh -c 'echo RECIPE-OK; ping -c1 -W1 8.8.8.8 ...'
RECIPE-OK
NET-DOWN      # network namespace is present and empty
```

This demonstrates the namespace-level containment primitive only. The
remaining elements of the approved v0.3 profile were **not** exercised
in the guest: credential/environment hygiene, the profile's fixed
mount set (the `--ro-bind`/`--tmpfs` mounts above are illustrative,
not the approved set), the mandatory N1 model-API CONNECT proxy (the
probe command neither bind-mounts the proxy socket nor runs the
in-sandbox relay — under the approved N1 design an `--unshare-net`
sandbox still reaches the out-of-sandbox proxy through a
bind-mounted Unix socket, so the unshared network is not what
excluded the proxy here; the probe simply never provided the path),
and the preflight negative probes. Those stay unverified
pending #372, matching the caveats in "Honest limits and non-claims".

Two Ubuntu 24.04-specific provisioning facts, measured not assumed:

- `kernel.apparmor_restrict_unprivileged_userns = 1` is the stock
  default; out of the box `bwrap` fails with `setting up uid map:
  Permission denied`. The fix is a scoped AppArmor profile granting
  `userns` to `/usr/bin/bwrap` only (in the recipe's `provision:`
  block) — not the broader `sysctl kernel.apparmor_restrict_unprivileged_userns=0`.
- `bubblewrap` (0.9.0) is not in the minimal image and is installed by
  the same provision step.

### Start/stop and memory release

- Stop is ~1.3 s and removes **all** host processes (hostagent, qemu,
  ssh mux verified absent via `pgrep`). Release evidence: `free %`
  recovered toward baseline; compressor pages decay lazily (macOS
  behaviour, not a leak — pages remain compressed until demanded).
- On-demand start cost is ~10 s warm for vz, ~7 s warm for qemu; the
  first-ever start is ~48 s image download (one-time, cached across
  instances) plus ~30–60 s first-boot cloud-init/provisioning.

## Supported Linux (documented, not run)

On a supported-Linux remote endpoint **no guest is needed**, and this
spike claims that honestly rather than inventing one: the v0.3
containment primitives (unprivileged user/mount/PID/network/IPC
namespaces via `bwrap`, environment hygiene, the CONNECT egress proxy)
are the profile's mechanisms — the namespaces are native kernel
features, while environment hygiene and the CONNECT egress proxy are
user-space components of the same approved profile — see
`docs/worker-protocol.md` selected profile and `docs/v0.3.md` OP-1
("Linux POSIX host with unprivileged user namespaces, `bwrap`, and the
taisan CONNECT proxy"). `docs/v0.3.md` records the #259/#260
mechanisms as landed with contract/fixture evidence — the containment
preflight battery covers namespace enforcement but does not exercise
the CONNECT proxy leg (`proxy_sock` is `None` there), and owner-run
live acceptance is still pending — so this is the specified profile
with mechanism-level evidence, not live-accepted or fully-exercised
enforcement. A VM or
container daemon on such a host would
add a resident daemon and memory for zero contract benefit. What the
endpoint needs beyond the existing v0.3 primitives is a small
restricted transport helper (ssh or equivalent) and separate
roots/storage — the protocol-level proof (request transfer, artifacts,
reconnect, negative probes over separate roots and real transport) is
**provisional** on #372's endpoint spec and requires a second Linux
machine this spike does not have.

## Ranking

By total host-visible incremental resident memory first, then secondary
criteria:

1. **Lima `vmType: vz` — selected.** ~100 MB idle host footprint for a
   1 GiB-capped guest; ~10 s warm start; no host sudo, no daemon, no
   Docker; starts on demand and fully releases; runs the v0.3 bwrap
   namespace-level boundary after a scripted one-time provision.
2. **Lima `vmType: qemu` — fallback.** Same arm64 guest, same tooling,
   but ~1.84 GB resident at idle — the fallback for arm64 hosts where
   vz is unavailable (e.g. Apple Silicon on macOS < 13). Keep as
   documented fallback, not default; no Intel recipe is pinned in this
   round — qemu on Intel cannot boot an arm64 guest efficiently.
3. **Firecracker — unavailable on macOS** (Linux/KVM-only); on a Linux
   endpoint it would compete with the native-bwrap path that needs no
   guest at all, so it ranks below "no guest" there too.
4. **OrbStack — excluded by the issue's boundary** (#373 requires the
   reference environment not to depend on Docker; OrbStack's model is a
   resident Docker daemon plus an always-on shared VM — ~384 MB
   resident measured above — and a commercial licence for non-personal
   use makes it unsuitable as the project default). Recorded as
   inventory context only.

## Pinned default recipe

```yaml
# taisan remote-execution reference environment — Lima / vz
# Host: macOS >= 13 on Apple Silicon (arm64) — this recipe is arm64-only
# (aarch64 guest image, vz; no Intel recipe is pinned). Guest: Ubuntu 24.04 minimal.
# Tested: arm64, macOS 26.7.1.
vmType: vz
os: Linux
arch: aarch64
images:
  - location: "https://cloud-images.ubuntu.com/minimal/releases/noble/release-20261105/ubuntu-24.04-minimal-cloudimg-arm64.img"
    arch: aarch64
    digest: "sha256:3dc8370d923978d4b610fa7964d1b36ba040eefb43198809ca5bc8ac51777b18"
cpus: 1
memory: "1GiB"      # measured floor: 512MiB OOM-kills a 400MB working set
disk: "10GiB"
mounts: []          # no ambient home mount; artifacts move over transport
mountType: virtiofs
containerd:
  system: false
  user: false
vmOpts:
  vz:
    rosetta:
      enabled: false
      binfmt: false
video:
  display: "none"
upgradePackages: false
provision:
  - mode: system
    script: |
      #!/bin/bash
      set -eux
      apt-get update
      apt-get install -y bubblewrap
      cat >/etc/apparmor.d/bwrap <<'AA'
      abi <abi/4.0>,
      include <tunables/global>

      profile bwrap /usr/bin/bwrap flags=(unconfined) {
        userns,
        include if exists <local/bwrap>
      }
      AA
      apparmor_parser -r /etc/apparmor.d/bwrap
```

One-time host steps: `brew install lima`; `limactl create --name
<name> ./recipe.yaml`; `limactl start <name>` (≈48 s image download
once, then ~60 s first boot with provisioning). Routine lifecycle is
`limactl start|shell|stop <name>` — no sudo, no persistent daemon, no
Docker.

Notes on the pinned values:

- `memory: "1GiB"` is the measured safe floor: 512 MiB boots and runs
  `bwrap` but the kernel OOM-kills a 400 MB working set — too thin for
  any realistic executor payload. Under vz the configured value is a
  cap, not a reservation, so raising it (e.g. to 2 GiB) costs almost
  nothing at idle; whether real executor workloads need more than 1 GiB
  is **provisional** on #372's executor spec.
- `mounts: []` keeps the no-shared-filesystem postcondition: artifacts
  cross via the transport helper, not an ambient mount.
- The AppArmor profile is the scoped fix for Ubuntu 24.04's default
  userns restriction; it does not weaken the restriction for other
  binaries inside the guest.

## Honest limits and non-claims

- **Co-resident execution tests remote semantics; it does not offload
  host compute.** The guest runs on the same 8 GiB machine — its CPU
  and ~100 MB+compressed-pressure memory are spent locally. The
  recipe's value is that the *same protocol* must later target a
  second Linux machine without provider coupling (#372 scope).
- This spike measured environment cost and proved the v0.3 boundary
  primitive inside the guest. It did **not** run request transfer,
  disconnect/reconnect, artifact retrieval, or negative
  filesystem/process/network probes end-to-end — those acceptance rows
  belong to the #372-defined endpoint and need separate roots and real
  transport.
- vz per-process accounting under-reports guest-touched memory;
  system-level deltas on a live workstation carry unrelated churn
  (measurements above state both honestly). The ranking does not depend
  on the attribution — qemu's resident cost is higher under either
  reading (~18x the vz idle process footprint, ~2x under the worst-case
  compressor-inclusive accounting).
- Measured on a single arm64 Mac; the pinned recipe requires an Apple
  Silicon host — Intel Macs are out of scope for this recipe, and
  Intel-Mac vz behaviour and a Linux-host comparison are unmeasured
  (honest absence, not a claim).
- Minimum safe memory: 1 GiB guest is the measured floor for this
  spike's trivial workload. If the eventual executor's real working set
  exceeds a realistic laptop budget, that finding must be surfaced at
  #372 integration time, not hidden by this recipe — the recipe makes
  headroom cheap, it does not prove workloads fit.

## Provisional pending #372

| Open decision | What this spike established | What depends on #372 |
| --- | --- | --- |
| Executor/Worker placement | the guest can host them (bwrap works, ~360–770 MB available at 1 GiB) | whether one guest suffices or a distinct executor guest is needed — the issue's "inspect actual coordinator portability" clause |
| Coordinator placement | coordinator stays native on the host; nothing requires it guest-side | confirmation in the endpoint spec |
| Transport helper | ssh transport exists and is measured (ControlMaster ~1 MB) | the restricted-helper contract |
| Second-Linux-machine proof | documented analysis only | separate-roots/real-transport evidence on a real second host |
