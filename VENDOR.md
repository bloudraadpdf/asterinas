# Bloudraad Asterinas patches

Tokoloshe consumes `patch/v0.19.0`, based on upstream `v0.19.0`.
Follow the [organisation fork standard](https://github.com/bloudraadpdf/mzansi/blob/main/docs/reference/forks.md).

The initramfs patch preserves CPIO ownership, modes, timestamps and regular-file
hard links. Directory metadata is applied after extraction. An extraction error
stops boot instead of admitting a partial application filesystem.

The consumer regression uses Tokoloshe's production image writer and bootstrap
assembler. Run `just build-asterinas-evaluation` and `just qualify-asterinas-boot`
in its Asterinas worktree. The regression checks file bytes, ownership, set-ID
modes, directory metadata, symbolic-link ownership, hard-link identity and
modification times. The unpatched kernel fails the metadata, link and time
checks; the patched kernel passes all three.

The CPIO contract also applies to the existing Unikraft `lib/ukcpio` port.
This patch uses Asterinas VFS operations and adds no unsafe kernel code.
Upstream submission is pending.

The OSDK lockfile patch adds `--lockfile PATH` for the generated kernel crate.
OSDK copies that file into the generated crate and invokes Cargo with `--locked`.
Tokoloshe owns this lockfile separately from the OSDK builder lockfile. A stale
kernel lockfile fails before compilation. The OSDK unit tests and Clippy pass.
The CLI resolves lockfile paths before entering the generated crate and
rejects missing files and directories without a panic. Installation reads
the input before writing the generated lockfile, so equal paths and hard links
cannot truncate the input. Regression tests cover invalid paths, stale locks
and aliased input and output files.

The descriptor patch honours `MSG_CMSG_CLOEXEC` when `recvmsg` installs
`SCM_RIGHTS` descriptors. Flags are set during file-table insertion and returned
in `msg_flags`. Tokoloshe's HTTP launcher requires this property for its
notification and release descriptors. The upstream Unix-stream regression
checks both flag values. The consumer descriptor-handoff and release-pipe probe
fails without the patch and passes on AMD64 Firecracker with it.

The optional `http_listen` feature ports Unikraft's `/dev/http-listen` contract:
root-only access, one two-byte network-order port request and a poll notification
when an IPv4 TCP listener enters LISTEN on that port. Notification runs after
socket locks are released. The optional `privileged_vsock` feature requires
`CAP_NET_ADMIN` in the initial user namespace before vsock socket creation.
Separate application processes retain the kernel's normal credential checks.
The AMD64 consumer probe checks notification delivery, wrong-port and invalid
requests, unprivileged vsock denial, privilege escalation and access to the
privileged process's descriptors. Kernel Clippy passes with both features.

PVH keeps reserved memory reserved even when the same region contains an ACPI
root table. Firecracker also stores live clock and generation pages there.
OSTD's read-only `SharedFirmwareMemory` API accepts only fully reserved pages
in the x86 firmware window. It rejects overlaps with live memory and exposes no
write API. Its mapping checks pass as an OSTD kernel test under Firecracker.

OSTD's `rcu_barrier` waits for queued and running callbacks, including callbacks
queued by another callback. The monitor counts a callback until its body returns,
so completion of a later grace period cannot hide an earlier running callback.
The nested-callback OSTD test passes under Firecracker.

The optional AMD64 `vm_restore` feature ports the VMClock and VMGenID device
ABIs from Unikraft. The devices check the clock ABI, use sequence reads, and
supply descriptor events, refresh and quiescence operations. Random output checks
the shared generation before use; restore refresh requires fresh hardware entropy
and completion of the vsock connection reset. Clock interrupts reseed before
notification. The driver boots under Firecracker. Snapshot qualification remains
pending; this feature does not establish Tokoloshe runtime compatibility.
