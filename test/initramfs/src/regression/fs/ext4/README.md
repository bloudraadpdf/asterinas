# Ext4 VFS regression

Build with the regression suite. Enable `fs_ext4` and the block-device driver.
Pass an empty, disposable ext4 device explicitly:

```sh
/test/fs/ext4/vfs /dev/vdd
```

The test mounts the device at `/tmp/ext4-vfs`, writes files, exhausts free space,
reclaims an open unlinked file, then unmounts and opens a read-only mount.
It covers exact EOF, shared mappings, truncate/regrow, partial user-buffer
faults, inode identity, attributes, metadata times, links, pipes and temporary
files. The mountpoint must not exist. `/tmp` must exist. After the test, detach
the disk and run Linux `e2fsck -fn` on the output image.

Use 1, 2 and 4 KiB block sizes. The native disk-library tests separately cover
journal and on-disk corruption cases. Block-I/O failure injection through the
VFS, additional architectures and SMP need separate qualification.

The default regression runner does not attach an ext4 scratch device and does
not run this test automatically.
