# File and Directory Operations

<!--
Put system calls such as
open, openat, creat, close, read, write, readv, writev, pread64,
pwrite64, lseek, stat, fstat, lstat, statx, mkdir, rmdir, link,
unlink, rename, symlink, readlink, chmod, fchmod, chown, fchown,
utime, and utimensat
under this category.
-->

### `open` and `openat`

Supported functionality of `open` in SCML:

```c
{{#include open_and_openat.scml}}
```

Silently-ignored flags:
* `O_NOCTTY`
* `O_DSYNC`
* `O_SYNC`
* `O_LARGEFILE`
* `O_NOATIME`
* `O_NOCTTY`

Partially-supported flags:
* `O_PATH`
* `O_TMPFILE` (only on file systems that support unnamed temporary files, e.g. ramfs)

Supported and unsupported functionality of `openat` are the same as `open`.
The SCML rules are omitted for brevity.

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/openat.2.html).

### `openat2`

```c
{{#include openat2.scml}}
```

The syscall validates all 64 bits of the open flags, mode and resolution flags.
The initial `open_how` is 24 bytes.
Larger structures require zero extension bytes and cannot exceed one page.
Unknown flags, invalid creation modes, incompatible `O_PATH` flags,
and `RESOLVE_BENEATH | RESOLVE_IN_ROOT` return `EINVAL`.
Descriptor reservation precedes file creation or truncation;
failed opens release their reservation.

`RESOLVE_BENEATH` anchors relative traversal at `dirfd` and rejects escape with `EXDEV`.
`RESOLVE_IN_ROOT` instead rebases absolute paths and absolute symlinks there,
and clamps `..` at that root.
`RESOLVE_NO_XDEV` rejects ordinary and bind-mount transitions.
The symlink and magic-link restrictions apply while traversing links;
an `O_PATH | O_NOFOLLOW` open can still return the final link itself.
Underlying filesystem support for open flags remains the same as for `openat`.

`RESOLVE_CACHED` uses nonblocking dentry-cache and inode metadata/link queries.
RAMfs implements resident metadata and symlink queries with try-locks.
A cache miss, required revalidation, unavailable resident metadata,
or a contended resolver/cache/topology lock returns `EAGAIN`.
Creation, truncation and temporary-file requests also return `EAGAIN` with this flag.
Other filesystems can implement the same cache-only inode APIs;
their default answer is that resident data is unavailable.

Constrained walks pin path objects and snapshot the mount generation under a brief read guard.
They release that guard before calling filesystem lookup or magic-link callbacks.
The topology write guard marks its generation odd during a mutation and even on release.
A changed or odd generation invalidates the walk with `EAGAIN`,
including concurrent bind mounts and pivot-root changes.
Each dentry rename marks a generation and active-writer count before changing parents,
including nested overlay renames and all error exits through an RAII guard.
A scoped reader starts only with no active writer,
checks the generation while walking parents,
then requires no active writer and the same generation at completion.
Sequentially consistent atomics order these checks and mutation markers.
Any overlapping rename returns `EAGAIN`.
Neither a global tree reader nor a mount-topology reader is held across filesystem callbacks,
so overlay copy-up can rename and procfs magic links can acquire a target resolver.
Access mode 3 checks read/write permission but permits only descriptor control operations.
It does not permit data I/O and cannot open directories or FIFOs.

The final pinned path or creation parent was inside the root during resolution;
a later rename can move it, as on Linux.

See the Linux [`openat2` ABI](https://man7.org/linux/man-pages/man2/openat2.2.html)
and [scoped path-walk contract](https://github.com/torvalds/linux/blob/v6.16/fs/namei.c#L903-L921).

### `renameat2`

Supported functionality in SCML:

```c
{{#include renameat2.scml}}
```

Unsupported flags:
* `RENAME_EXCHANGE`
* `RENAME_NOREPLACE`
* `RENAME_WHITEOUT`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/rename.2.html).

### `lseek`

Supported functionality in SCML:

```c
{{#include lseek.scml}}
```

Unsupported flags:
* `SEEK_DATA`
* `SEEK_HOLE`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/lseek.2.html).

### `newfstatat`

Supported functionality in SCML:

```c
{{#include newfstatat.scml}}
```

Silently-ignored flags:
* `AT_NO_AUTOMOUNT`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/newfstatat.2.html).

### `preadv2` and `pwritev2`

Supported functionality in SCML:

```c
{{#include preadv2_and_pwritev2.scml}}
```

Silently-ignored flags:
* `RWF_DSYNC`
* `RWF_HIPRI`
* `RWF_SYNC`
* `RWF_NOWAIT`

Unsupported flags:
* `RWF_APPEND`
* `RWF_NOAPPEND`
* `RWF_ATOMIC`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/preadv2.2.html).

### `faccessat2`

Supported functionality in SCML:

```c
{{#include faccessat2.scml}}
```

Silently-ignored flags:
* `AT_EACCESS`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/faccessat2.2.html).

### `statx`

Supported functionality in SCML:

```c
{{#include statx.scml}}
```

Silently-ignored flags:
* `AT_NO_AUTOMOUNT`
* `AT_STATX_FORCE_SYNC`
* `AT_STATX_DONT_SYNC`

Silently-ignored masks:
* `STATX_DIOALIGN`
* `STATX_SUBVOL`
* `STATX_WRITE_ATOMIC`
* `STATX_DIO_READ_ALIGN`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/statx.2.html).

### `fallocate`

Supported functionality in SCML:

```c
{{#include fallocate.scml}}
```

Unsupported modes:
* `FALLOC_FL_UNSHARE_RANGE`
* `FALLOC_FL_COLLAPSE_RANGE`
* `FALLOC_FL_ZERO_RANGE`
* `FALLOC_FL_INSERT_RANGE`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/fallocate.2.html).

### `utimensat`

Supported functionality in SCML:

```c
{{#include utimensat.scml}}
```

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/utimensat.2.html).
