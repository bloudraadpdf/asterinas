# Running Asterinas on Firecracker

Asterinas can run as a guest kernel inside a
[Firecracker](https://github.com/firecracker-microvm/firecracker) microVM
through the PVH boot protocol.
This page shows how to build the kernel and boot it with Firecracker.

## Prerequisites

- An x86-64 host with KVM enabled (`/dev/kvm` accessible).
- A `firecracker` binary in `PATH`.
  The Asterinas Docker development image already has Firecracker installed.

## Build the kernel

```bash
make kernel BOOT_PROTOCOL=pvh
```

This builds a PVH-bootable kernel image.
`BOOT_PROTOCOL=pvh` automatically uses the `vmm-direct` boot method
and enables the `pvh_boot` feature.

For a small guest RAM allocation, also enable `pvh_boot_only` and disable
all default features:

```bash
OSTD_TASK_STACK_SIZE_IN_PAGES=32 cargo osdk build \
    --target-arch x86_64 --boot-method vmm-direct --profile release-lto \
    --features pvh_boot_only,log_max_level_info --no-default-features
```

This feature loads the kernel at 16 MiB and removes the Linux and Multiboot
entry points and headers. OSDK rejects Linux and GRUB image packaging for this
kernel. Builds without `pvh_boot_only` keep the Linux entry points at `0x8001000` and `0x8001200`.
The guest must still have enough RAM for its kernel, archive and application.

In `pvh_boot_only` builds, OSTD owns the boot archive until the kernel has
unpacked it. Dropping that owner clears and registers its full physical pages
with the frame allocator. Shared edge pages and pages that overlap retained
boot strings stay reserved. Other boot configurations retain their archive
reservation. `BootInfo::take_initramfs()` transfers the archive once; byte
borrows cannot outlive its owner.

The `release-lto` profile uses full LTO and release optimisation. The example
reserves 128 KiB per task stack and retains its guard pages.
`log_max_level_info` removes kernel debug messages at compile time and keeps
informational messages, warnings and errors.

The default features retain the general-purpose device and filesystem set.
A build with `--no-default-features` retains the serial console, loopback,
virtio-MMIO entropy and vsock, CPIO, and memory and pseudo filesystems.
The optional component features are:

| Feature | Components |
| --- | --- |
| `console_video` | DRM, framebuffer, virtual terminals, keyboard and input devices |
| `storage_nvme` | NVMe driver and request workers |
| `filesystems_extra` | ext2, exFAT, overlayfs, configfs and virtiofs |
| `virtio_devices` | virtio block, console, filesystem, input and network devices |
| `virtio_pci` | virtio PCI transport |

`filesystems_extra` enables `virtio_devices` for virtiofs. The `cvm_guest`
feature enables `filesystems_extra` for its configfs interface.
Process isolation, namespaces and cgroup accounting remain available in a
minimal build. The default console is `ttyS0` when `console_video` is disabled.

The output kernel image is:

```text
target/osdk/asterinas/asterinas-osdk-bin.elf
```

The default initramfs used for the boot test is:

```text
test/initramfs/build/initramfs.cpio.gz
```

## Boot with Firecracker

Create a Firecracker VM configuration file, for example `vm_config.json`:

```json
{
  "boot-source": {
    "kernel_image_path": "target/osdk/asterinas/asterinas-osdk-bin.elf",
    "initrd_path": "test/initramfs/build/initramfs.cpio.gz",
    "boot_args": "console=ttyS0 rdinit=/bin/sh"
  },
  "drives": [],
  "machine-config": {
    "vcpu_count": 2,
    "mem_size_mib": 1024
  }
}
```

The `rdinit=/bin/sh` kernel parameter in `boot_args`
selects the init program to run as the first userspace process;
here it starts a BusyBox shell on the serial console.
Omitting `rdinit` boots the default initramfs `/init` instead.

Then boot the microVM:

```bash
firecracker --no-api --config-file vm_config.json
```
