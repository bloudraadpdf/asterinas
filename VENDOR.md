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
notification. The driver boots under Firecracker. Tokoloshe's qualification
records cover AMD64 snapshot restore and single-use HTTP jobs. Each result
applies to its recorded source revision. Confidential-VM execution is not qualified.

`clock_settime(CLOCK_REALTIME)` requires `CAP_SYS_TIME` in the initial user
namespace. It rejects malformed times, values outside Linux's clock range and
values before the monotonic clock. Clock changes update the syscall, coarse,
`/proc/stat` boot-time and vDSO values; monotonic clocks retain their timebase.
The consumer ABI probe fails with `ENOSYS` before this patch and passes under
Firecracker with it. Checks include forward and backward changes, fractional
seconds, vDSO reads, monotonic clocks, rejected inputs and unprivileged denial.
See the [Linux time-setting contract](https://kernel.googlesource.com/pub/scm/linux/kernel/git/torvalds/linux/+/refs/heads/master/kernel/time/timekeeping.c).

Vsock stream sockets apply `SO_RCVTIMEO` to accept and reads, and
`SO_SNDTIMEO` to connect and writes. Accepted sockets inherit both timeouts.
`MSG_DONTWAIT` bypasses blocking waits. The consumer checks option values,
accept and read expiry, inheritance and nonblocking reads before exchanging
bytes with the host.

Route netlink replies identify the recipient in their outer header and keep
the original request header in errors. Unsupported link mutations still fail.
The consumer verifies the preconfigured loopback state through `RTM_GETLINK`;
Linux continues to initialise its loopback interface through `RTM_NEWLINK`.

The optional AMD64 `pvh_boot_only` feature loads the kernel at 16 MiB and
removes Linux and Multiboot entry points and headers. Builds without this
feature keep the Linux entry addresses. OSDK reads the linked ELF headers,
retains the PVH ELF machine type, and rejects Linux and GRUB image packaging
for a PVH-only kernel. Linked-assembly regression tests check both layouts
and the rejected image types. Tokoloshe uses this feature to permit a smaller
guest RAM allocation and decrease frame metadata. Upstream submission is pending.

The v0.19.0 base predates upstream permission fixes for chmod, chown and mount.
The fork backports upstream commits `21c0b04d3`, `9f53affb2`, `01acaa7ee`
and `dbf521a1b`, including their regression tests. The consumer's unprivileged
native-policy probe reproduces all three accepted operations before the fix
and verifies denial afterwards. It also checks setgid handling for another
group and set-ID removal on an ownership change. These changes are already
upstream and must not be submitted again.

The PID namespace patch adds hierarchical thread and process identifiers,
`CLONE_NEWPID`, `unshare` and `setns` support, and procfs mount scoping.
Namespace init exit closes clone admission before it terminates and reaps
members. Queued signals, Unix credentials, process groups and sessions retain
identities until their users release them. Nested namespaces reuse sparse
identifiers; root identifiers remain monotonic and reject exhaustion.
The implementation adds no unsafe kernel code.

The in-tree `pid_namespace` regression covers nested identifiers, procfs,
threads, namespace selection, failed init creation, permission checks, signals,
Unix credentials, descendant cleanup and concurrent clone/init exit. The same
12 cases pass on Linux and on four-vCPU AMD64 Firecracker guests. Existing
process, pidfd, signal, subreaper, exec, robust-mutex, clone and ptrace regressions
also pass. These process boundaries apply to Linux and Asterinas guests;
Unikraft retains its shared address space and VM boundary. This patch does not
change TDX or AMD confidential-VM code or establish confidential-VM execution
support.

The tmpfs patch parses size, inode, mode and ownership options for mounts.
Page and inode reservations use checked atomic counters and owned charges.
Writes and shared mapping faults use one page budget. Truncate removes mappings
before it releases charges; sparse reads leave holes unallocated. Failed and
partial writes preserve the file size and report the bytes written. Internal
ramfs, memfd and device backings retain their previous limits.
The eight Linux-reference cases also pass on four-vCPU Firecracker guests.
The checks cover exhaustion, release, sparse files, partial writes, live
unlinked inodes and concurrent writers. This patch adds no unsafe kernel code.

The configuration patch gives configfs its own feature. `cvm_guest` keeps
configfs, the TDX guest device, attestation and OSTD support without selecting
unrelated extra filesystems or Virtio devices. Default builds retain those
components through `filesystems_extra`. The reduced CVM profile, default and
ktest builds pass Clippy. Linked symbols include the TDX device and configfs.
Ordinary KVM checks do not establish TDX hardware execution. No AMD code changes.

The OSDK image installer writes an independent boot ELF and replaces its output
atomically. QEMU's ELF-header change can no longer modify a hard-linked compiler
artifact. This also handles an output left as a hard link by an earlier build.
The regression fails before the change and passes afterwards for both cases.
