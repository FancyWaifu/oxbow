# Access rules: where the namespace-policy system can go

> **Status (implemented):** the whole roadmap below is now built and verified —
> per-process confinement (`confine`), nested/single-file mounts, the rights set
> (ro/rw/append/list/nodelete), network as a governed resource, and per-program
> profiles + roles. See the "Suggested roadmap" section at the end for the
> per-item status, and the `grant`/`role`/`assign`/`profile`/`confine`/`rules`
> shell commands. This document is kept as the design rationale.


## What exists today (commits 7f956dc, ce5621e)

Root defines **access rules** that decide which top-level directories each user
can reach, and at what permission. The model is deliberately small:

- **Subject:** a user (uid) or a group (gid).
- **Resource:** a top-level directory name (e.g. `bin`, `docs`).
- **Right:** `ro` or `rw`.
- **`grant <user|@group> <dir> [ro|rw]`** / **`revoke`** / **`rules`**, root only.
- Persisted to `/etc/access.rules`, reloaded before the first login.

The enforcement is pure capability mechanics, which is the important part:

- The shell is the **policy authority**. At login it mints the user a *namespace
  capability* rooted at their home (`TAG_FS_NAMESPACE`), then **mounts** the
  permitted top-level dirs into it (`TAG_FS_NS_MOUNT`). A mounted component
  resolves against the real fs root (shared); everything else resolves under
  home (private).
- Every program the user spawns **inherits** the namespace as slot 1, so the
  rules apply to everything they run with zero per-program wiring.
- Read-only travels **with the capability**: a `RO_CAP` badge bit flags caps
  minted in a read-only location, propagates to every child cap opened through
  them, and is checked in every mutating fsd handler.

The key realisation: **a rule is just a recipe for which capabilities get
composed into the subject's namespace.** Expanding the system = widening three
axes — *subjects*, *resources*, and *composition* — without ever adding ambient
authority to the kernel.

---

## Axis 1 — Subjects: who/what a rule binds to

Today: user, group. Natural extensions, in increasing oxbow-nativeness:

### 1a. Per-process confinement (the "individual processes" idea) — **recommended first**

oxbow already has the right primitive: a process's authority *is* the slot-1
namespace cap it inherits, and the kernel already has **pledges** (the
attenuation law). So per-process access control = spawning a child with a
**narrower** namespace than its parent. Two concrete forms:

- **Drop (unveil-style):** a parent mints a fresh namespace cap, mounts only a
  subset of what it itself holds, and passes *that* as the child's slot 1 instead
  of its own session root. e.g. run a network daemon with `/etc` + `/var` mounted
  but **not** `/home`. This is exactly the existing `TAG_FS_NAMESPACE` +
  `TAG_FS_NS_MOUNT` + `RO_CAP` machinery, invoked per-spawn instead of per-login.
  A shell builtin could be: `confine <dir>[:ro] ... -- <cmd>`.
- **Monotonic narrowing only:** a child can never gain a mount its parent lacks
  (the parent can only mount dirs it can already name). This is capability
  monotonicity — it's what keeps per-process rules safe with no kernel check.

This is the highest value / lowest effort extension because the enforcement is
already built; only a per-spawn namespace-composition entry point is missing.

### 1b. Per-program profiles (AppArmor/firejail-style)

Rules keyed by the **executable path**: "`curl` gets `net` + `/tmp`, nothing
else." When the shell resolves a non-builtin to a `/bin` file, it looks up a
profile for that path and composes the profile's namespace for the child.
Subject kind `program <path>` added to the RULES table; matched at exec time.
Combines with 1a (the profile *is* a confinement recipe).

### 1c. Roles / named profiles

A named bundle of mounts (`role developer = {bin:ro, src:rw, build:rw}`) that
can be granted to a user or assumed for a session. Reduces rule sprawl; a single
`grant bryson @role:developer` expands to several mounts at login.

---

## Axis 2 — Resources: what a rule governs

Today: top-level filesystem directories. Because **everything in oxbow is a
capability**, the same "rules → composed capability set" model extends past the
filesystem. This is the big payoff.

### 2a. Finer filesystem grain

- **Nested paths**, not just top-level (`grant bryson projects/oxbow rw`). The
  namespace node would route a multi-component prefix, not just the first
  component. Modest change to `ns_mount_for`/`join_child`.
- **Single files** (mount one file, e.g. expose `/etc/resolv.conf` ro to a
  confined service).
- **Curated `/bin` subsets** — a *restricted shell*: mount a synthesized bin dir
  containing only the tools a user may run, instead of all of `/bin`. "OS tools
  for all users, but not every program for everyone" taken to the per-tool level.

### 2b. Network as a governed resource — **high value**

`BOOT_NET_EP` is currently handed to spawned programs unconditionally. A rule
`grant bryson net` would conditionally include/exclude the net endpoint from the
namespace. Finer: a **userspace net broker** that hands out per-host / per-port
socket caps the way fsd hands out per-path file caps — "may reach `*.whiskeyden.xyz:443`
only." Same shape as the fs namespace; a different server.

### 2c. Devices

fb / gpu / input / IRQ / IO-port caps are already capability-gated by *who the
kernel grants them to at boot*. A rule layer would decide which users/processes
get a framebuffer at all (a service account is headless), or read-only input,
etc. Mostly a matter of routing those caps through the same policy table.

### 2d. Resource budgets / quotas

The `Memory` budget cap is already a quantitative capability. A rule could cap a
subject's memory budget, max open files, or fs bytes written (the ext2 layer
would need byte accounting). Turns qualitative access into quantitative limits.

---

## Axis 3 — Rights & composition: richer than ro/rw

- **More rights:** append-only, exec-only / no-exec, list-only (readdir but not
  read), create-but-not-delete. Each is another bit (or small enum) in the badge
  alongside `RO_CAP`, checked in the matching handlers. The badge-carried model
  already proved this works for ro.
- **Deny rules:** today the system is allow-only (union of matching grants). A
  deny layer (with defined precedence: deny > allow, or longest-prefix wins) lets
  root carve holes in a broad grant.
- **Time-boxed / one-shot grants:** a grant that expires (needs a clock the shell
  trusts) or is consumed after first use.
- **Delegation:** let a *user* (not just root) grant a **subset** of what they
  hold to their own processes (1a is the mechanism; this is the policy that
  permits it). Safe by monotonicity.

---

## Suggested roadmap (value / effort) — all implemented

1. **Per-process confinement (1a)** — DONE. `confine <dir>[:right] ... -- <cmd>`
   mints a fresh namespace (home + /bin + listed dirs only) and runs the command in
   it. Non-root is limited to dirs it already holds (monotonicity); root may mount
   anything.
2. **Nested-path & single-file mounts (2a)** — DONE. `ns_mount_for` matches a path
   prefix component-wise (longest wins); a grant may be `projects/oxbow` or a file.
3. **More rights (3)** — DONE. The badge carries a rights value (FS_RIGHT_*):
   ro / rw / append / list / nodelete, checked per operation. (exec-only/no-exec was
   left out — exec reads bytes via the normal read path, so it isn't cleanly
   separable at the fs-cap layer; the `confine` curated-/bin approach covers the
   "which tools" need instead.)
4. **Network as a governed resource (2b)** — DONE (the gate). `BOOT_NET_EP` is handed
   to a spawned program only when the session/profile has a `net` rule (`grant <u>
   net`); root always has it. The per-host socket broker remains a future refinement.
5. **Per-program profiles & roles (1b/1c)** — DONE. `role <r> <dir|net> [right]` +
   `assign <user> <role>` for reusable bundles; `profile <prog> <dir|net> [right]`
   confines a named program at exec to home + /bin + the profile's mounts.

The throughline held: the **kernel** never learned about users, rules, or rights —
all of this lives in the userspace policy authority (the shell composing namespaces +
fsd enforcing badge-carried rights). Each axis was additive.

The throughline: keep the **kernel** capability-pure (it never learns about
"users" or "rules"), and keep growing the **userspace policy authority** (the
shell + servers) that decides which capabilities get composed into whom. That's
the Redox/seL4 split — policy in userspace, mechanism (unforgeable caps) in the
kernel — and it's why each of these extensions is additive, not a rewrite.
