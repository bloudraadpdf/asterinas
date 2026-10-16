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
