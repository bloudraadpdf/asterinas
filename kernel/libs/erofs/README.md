# EROFS reader

`aster-erofs` parses an immutable image through `io_util::ReadAt`. The kernel
adapter supplies block I/O, the VFS inode registry and demand paging. The reader
has no OSTD dependency and forbids unsafe Rust.

The current layouts are flat, inline, full compression indexes and compact
compression indexes. LZ4 uses one physical block per cluster. Unsupported
incompatible features and compression modes return a typed error. Directory
names and attribute names remain byte strings. The VFS adapter supplies its
existing string and permission rules.

The volume retains one physical block and a 64 KiB LZ4 history for sequential
reads. The caller supplies the output buffer. A read does not allocate a
decompressed copy of a complete file. The kernel serialises volume reads with a
sleeping mutex and releases it before it copies bytes to userspace.

Run the native tests with `cargo test -p aster-erofs`. The fixtures were made by
`mkfs.erofs` and checked by `fsck.erofs`. On Linux, generate into a new directory:

```sh
python3 kernel/libs/erofs/tests/generate_fixtures.py BUILD_DIRECTORY
```

Compare the three `.erofs` files with `tests/fixtures/`. The generator fixes the
UUID, ownership and timestamps. Keep the formatter version with the fixture
review record because its output can change between versions.

The compact-index algorithm is ported from the MIT-licensed
[`erofs-utils/lib/zmap.c`](https://github.com/erofs/erofs-utils/blob/5fcb2f76f7b95df87f5c6d3a580e4d756ceaa1c8/lib/zmap.c).
Its notices remain in `src/compression.rs` and `LICENSE-MIT`. Other reader code
uses the repository's MPL-2.0 licence.
