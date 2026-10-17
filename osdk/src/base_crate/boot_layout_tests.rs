// SPDX-License-Identifier: MPL-2.0

use std::{fs, path::PathBuf, process::Command};

use xmas_elf::{ElfFile, program::Type, sections::SectionData, symbol_table::Entry};

use crate::{
    bundle::bin::AsterElfMeta,
    config::scheme::{BootMethod, BootProtocol},
};

fn boot_elf(pvh_only: bool) -> Vec<u8> {
    let work = tempfile::tempdir().unwrap();
    let mut assembly = String::from(".intel_syntax noprefix\n");
    assembly.push_str(
        &include_str!("../../../ostd/src/arch/x86/boot/bsp_boot.S")
            .replace("{PVH_BOOT_ONLY}", if pvh_only { "1" } else { "0" })
            .replace("{KCODE64}", "0x00af9b000000ffff")
            .replace("{KDATA}", "0x00cf93000000ffff")
            .replace("{KCODE32}", "0x00cf9b000000ffff"),
    );
    if pvh_only {
        assembly.push_str(include_str!("../../../ostd/src/arch/x86/boot/pvh/note.S"));
    } else {
        assembly.push_str(include_str!(
            "../../../ostd/src/arch/x86/boot/multiboot/header.S"
        ));
        assembly.push_str(include_str!(
            "../../../ostd/src/arch/x86/boot/multiboot2/header.S"
        ));
    }
    assembly.push_str(
        "\n.text\n.code64\n.global __pvh_entry, __linux_boot, __multiboot_entry, __multiboot2_entry\n\
         __pvh_entry:\n__linux_boot:\n__multiboot_entry:\n__multiboot2_entry:\nret\n\
         .data\n.global __cpu_local_start\n__cpu_local_start: .quad 0\n",
    );
    fs::write(work.path().join("boot.S"), assembly).unwrap();
    fs::write(
        work.path().join("kernel.ld"),
        include_str!("x86_64.ld.template"),
    )
    .unwrap();
    let output = Command::new("rustc")
        .args(["--print", "target-libdir"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let libdir = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    let linker = libdir.parent().unwrap().join("bin/rust-lld");
    for (program, args) in [
        (
            PathBuf::from("clang"),
            vec![
                "--target=x86_64-unknown-none",
                "-c",
                "boot.S",
                "-o",
                "boot.o",
            ],
        ),
        (
            linker,
            vec![
                "-flavor",
                "gnu",
                "-T",
                "kernel.ld",
                "boot.o",
                "-o",
                "kernel.elf",
            ],
        ),
    ] {
        let output = Command::new(&program)
            .args(args)
            .current_dir(work.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {}",
            program.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let path = work.path().join("kernel.elf");
    let meta = AsterElfMeta::read(&path);
    assert_eq!(meta.is_pvh_only(), pvh_only);
    assert!(
        meta.validate_boot(BootMethod::VmmDirect, BootProtocol::Multiboot2, false)
            .is_ok()
    );
    for (method, protocol, legacy) in [
        (BootMethod::VmmDirect, BootProtocol::Linux, false),
        (BootMethod::VmmDirect, BootProtocol::Multiboot2, true),
        (BootMethod::GrubRescueIso, BootProtocol::Multiboot, false),
        (BootMethod::GrubQcow2, BootProtocol::Multiboot2, false),
    ] {
        assert_eq!(
            meta.validate_boot(method, protocol, legacy).is_err(),
            pvh_only
        );
    }
    fs::read(path).unwrap()
}

fn symbol(elf: &ElfFile<'_>, name: &str) -> Option<u64> {
    let SectionData::SymbolTable64(symbols) =
        elf.find_section_by_name(".symtab")?.get_data(elf).ok()?
    else {
        panic!("ELF has no 64-bit symbol table");
    };
    symbols
        .iter()
        .find(|entry| entry.get_name(elf) == Ok(name))
        .map(|entry| entry.value())
}

fn assert_layout(elf: &ElfFile<'_>, base: u64, entry: &str) {
    let load = elf
        .program_iter()
        .find(|segment| segment.get_type() == Ok(Type::Load))
        .unwrap();
    assert_eq!(load.physical_addr(), base);
    assert_eq!(elf.header.pt2.entry_point(), symbol(elf, entry).unwrap());
}

#[test]
fn pvh_only_loads_below_linux_entries_and_rejects_linux_and_grub_images() {
    let bytes = boot_elf(true);
    let elf = ElfFile::new(&bytes).unwrap();
    assert_layout(&elf, 0x1000000, "__pvh_boot");
    for name in ["__linux32_boot", "__linux64_boot", "__multiboot_boot"] {
        assert_eq!(symbol(&elf, name), None);
    }
}

#[test]
fn normal_layout_keeps_linux_boot_entry_addresses_and_multiboot_headers() {
    let bytes = boot_elf(false);
    let elf = ElfFile::new(&bytes).unwrap();
    assert_layout(&elf, 0x8000000, "__multiboot_boot");
    assert_eq!(symbol(&elf, "__linux32_boot"), Some(0x8001000));
    assert_eq!(symbol(&elf, "__linux64_boot"), Some(0x8001200));
}
