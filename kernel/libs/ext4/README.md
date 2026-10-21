# Checked ext4 I/O

`aster-ext4` parses and updates ext4 metadata in safe Rust
without an OSTD dependency.
The kernel's `fs_ext4` feature connects write sessions to the VFS.
The filesystem remains under development.

The reader supports 1, 2 and 4 KiB blocks, 64-bit descriptors, metadata
checksums, checksum seeds, extent trees and legacy indirect blocks. Unknown
incompatible or read-only-compatible features are rejected.

The write session supports regular files, directories, symbolic links, hard
links, atomic rename/replacement, special files, temporary files, attributes,
permissions, ownership and timestamps. It uses hash lookup and incremental
HTree leaf/interior splits, including root growth, and retains the index during
mutation and cross-parent moves. Immutable and append-only flags constrain
mutation.

Bounded writes support sparse extension, incremental extent allocation and
truncation. Allocation bitmaps and reserved metadata are checked. File data is
durable before metadata is journalled. Unlink retains open inodes until the
VFS calls `reap` after the last reference. An I/O failure seals the session.
Inline and external attributes share one checked codec; updates use copy-on-write
for shared external blocks.

Recovery validates JBD2 descriptors, data, commits and revocations before home
writes. It supports legacy transaction CRC and both CRC32C formats, classic
orphan lists and checksummed orphan files. Recovery resumes linked truncation
and reclaims all supported unlinked inode types. `MountLimits` bounds replay,
allocation scans, orphan retention and each owned transaction.

Mutation and recovery tests cover 1, 2 and 4 KiB blocks. A clean journal-free
volume can be written and closed; an unclean one requires external recovery.
Non-extent allocation/reclamation, extended HTree depth (`large_dir`),
and preallocation/hole punching remain unsupported.
The VFS adapter supplies inode identity, page caching,
mount admission and writeback error retention. Each VFS volume caches at most
1 MiB of metadata through `io-util::metadata_cache::MetadataCache`. Writes
update cached bytes; failed writes discard them. Regular-file data bypasses
this cache, including `O_DIRECT` reads. Unsupported
recovery retains the volume's recovery flags.

The directory hash port derives from lwext4's BSD-licensed `ext4_hash.c`.
Its half-MD4 transform is derived from the RSA Data Security, Inc. MD4
Message-Digest Algorithm. The original notices are in
[LICENSE-directory-hash](LICENSE-directory-hash).

The disk layouts follow the Linux documentation for
[ext4](https://www.kernel.org/doc/html/latest/filesystems/ext4/index.html),
[JBD2](https://www.kernel.org/doc/html/latest/filesystems/ext4/journal.html)
and [orphan files](https://www.kernel.org/doc/html/latest/filesystems/ext4/orphan.html).

Run native checks in the project development container:

```sh
cargo test --locked -p aster-ext4 -p io-util
cargo clippy --locked -p aster-ext4 -p io-util --all-targets -- -D warnings
```

Generate the sparse fixture on Linux with e2fsprogs 1.47.2:

```sh
python3 kernel/libs/ext4/tests/generate_fixtures.py .build/ext4-fixture
```

Two separate runs must produce byte-identical `.ext4` and `.sparse` files.
The generator fixes the filesystem UUID, hash seed and check times. It includes
checksummed and checksum-free images, both orphan formats, an indexed directory,
lazy non-zero inode tables and private/shared attribute blocks.
The committed sparse fixture is a byte-exact encoding:
an `EXT4SP01` header, little-endian device length (`u64`) and record count
(`u32`), then records with byte offset (`u64`), length (`u32`) and data.
Omitted bytes are zero. The base journal fixture uses inode 13 for `/payload`. Resolve the path in other fixtures.

Set `ASTER_EXT4_RECOVERY_OUTPUT` to an existing empty build directory when
running the recovery tests to export before/after images for Linux
interoperability checks. `e2fsck -fy` must recover each before image to the
expected payload; `e2fsck -fn` must accept each after image without repairs.

The `recover` example replays an image in place. Use a separate writable copy:

```sh
cargo run --locked --release -p aster-ext4 --example recover -- .build/recovery-copy.ext4
```

For Linux-produced recovery inputs, mount a fixture in a Linux VM, unlink
`/payload` while retaining an open descriptor, call `fsync` and `syncfs`, then
stop the VM without a clean unmount. Test both the orphan-file fixture and the
classic-list fixture. After Rust recovery, `e2fsck -fn` must require no repairs,
`/payload` must be absent, and inode 13 must be unallocated.
