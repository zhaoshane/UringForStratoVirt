// Copyright (c) 2020 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

use util::byte_code::ByteCode;

pub const E820_RAM: u32 = 1;
pub const E820_RESERVED: u32 = 2;

// Structures below sourced from:
// https://www.kernel.org/doc/html/latest/x86/boot.html
// https://www.kernel.org/doc/html/latest/x86/zero-page.html
#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone)]
pub struct RealModeKernelHeader {
    setup_sects: u8,
    root_flags: u16,
    syssize: u32,
    ram_size: u16,
    vid_mode: u16,
    root_dev: u16,
    boot_flag: u16,
    jump: u16,
    header: u32,
    version: u16,
    realmode_swtch: u32,
    start_sys_seg: u16,
    kernel_version: u16,
    type_of_loader: u8,
    loadflags: u8,
    setup_move_size: u16,
    code32_start: u32,
    ramdisk_image: u32,
    ramdisk_size: u32,
    bootsect_kludge: u32,
    heap_end_ptr: u16,
    ext_loader_ver: u8,
    ext_loader_type: u8,
    cmdline_ptr: u32,
    initrd_addr_max: u32,
    kernel_alignment: u32,
    relocatable_kernel: u8,
    min_alignment: u8,
    xloadflags: u16,
    cmdline_size: u32,
    hardware_subarch: u32,
    hardware_subarch_data: u64,
    payload_offset: u32,
    payload_length: u32,
    setup_data: u64,
    pref_address: u64,
    init_size: u32,
    handover_offset: u32,
    kernel_info_offset: u32,
}

impl RealModeKernelHeader {
    pub fn new(cmdline_ptr: u32, cmdline_size: u32, ramdisk_image: u32, ramdisk_size: u32) -> Self {
        RealModeKernelHeader {
            boot_flag: 0xaa55,
            header: 0x5372_6448,  // "HdrS"
            type_of_loader: 0xff, // undefined identifier and version
            cmdline_ptr,
            cmdline_size,
            ramdisk_image,
            ramdisk_size,
            ..Default::default()
        }
    }
}

#[repr(C, packed)]
#[derive(Debug, Default, Copy, Clone)]
pub struct E820Entry {
    addr: u64,
    size: u64,
    type_: u32,
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct BootParams {
    screen_info: [u8; 0x40],
    apm_bios_info: [u8; 0x14],
    pad1: u32,
    tboot_addr: [u8; 0x8],
    ist_info: [u8; 0x10],
    pad2: [u8; 0x10],
    hd0_info: [u8; 0x10],
    hd1_info: [u8; 0x10],
    sys_desc_table: [u8; 0x10],
    olpc_ofw_header: [u8; 0x10],
    ext_ramdisk_image: u32,
    ext_ramdisk_size: u32,
    ext_cmd_line_ptr: u32,
    pad3: [u8; 0x74],
    edid_info: [u8; 0x80],
    efi_info: [u8; 0x20],
    alt_mem_k: u32,
    scratch: u32,
    e820_entries: u8,
    eddbuf_entries: u8,
    edd_mbr_sig_buf_entries: u8,
    kbd_status: u8,
    secure_boot: u8,
    pad4: u16,
    sentinel: u8,
    pad5: u8,
    kernel_header: RealModeKernelHeader, // offset: 0x1f1
    pad6: [u8; 0x24],
    edd_mbr_sig_buffer: [u8; 0x40],
    e820_table: [E820Entry; 0x80],
    pad8: [u8; 0x30],
    eddbuf: [u8; 0x1ec],
}

impl ByteCode for BootParams {}

impl Default for BootParams {
    fn default() -> Self {
        unsafe { ::std::mem::zeroed() }
    }
}

impl BootParams {
    pub fn new(kernel_header: RealModeKernelHeader) -> Self {
        BootParams {
            kernel_header,
            ..Default::default()
        }
    }

    pub fn add_e820_entry(&mut self, addr: u64, size: u64, type_: u32) {
        self.e820_table[self.e820_entries as usize] = E820Entry { addr, size, type_ };
        self.e820_entries += 1;
    }
}

#[cfg(test)]
mod test {
    use std::path::PathBuf;
    use std::sync::Arc;

    use address_space::{AddressSpace, GuestAddress, HostMemMapping, Region};

    use super::super::{setup_boot_params, X86BootLoaderConfig};
    use super::*;

    #[test]
    fn test_boot_param() {
        // test setup_boot_params function
        let root = Region::init_container_region(0x2000_0000);
        let space = AddressSpace::new(root.clone()).unwrap();
        let ram1 = Arc::new(HostMemMapping::new(GuestAddress(0), 0x1000_0000, false).unwrap());
        let region_a = Region::init_ram_region(ram1.clone());
        root.add_subregion(region_a, ram1.start_address().raw_value())
            .unwrap();

        let config = X86BootLoaderConfig {
            kernel: PathBuf::new(),
            initrd: Some(PathBuf::new()),
            initrd_size: 0x1_0000,
            kernel_cmdline: String::from("this_is_a_piece_of_test_string"),
            cpu_count: 2,
        };
        let (_, initrd_addr_tmp) = setup_boot_params(&config, &space).unwrap();
        assert_eq!(initrd_addr_tmp, 0xfff_0000);
        let test_zero_page = space
            .read_object::<BootParams>(GuestAddress(0x0000_7000))
            .unwrap();
        assert_eq!(test_zero_page.e820_entries, 4);

        unsafe {
            assert_eq!(test_zero_page.e820_table[0].addr, 0);
            assert_eq!(test_zero_page.e820_table[0].size, 0x0009_FC00);
            assert_eq!(test_zero_page.e820_table[0].type_, 1);

            assert_eq!(test_zero_page.e820_table[1].addr, 0x0009_FC00);
            assert_eq!(test_zero_page.e820_table[1].size, 0x400);
            assert_eq!(test_zero_page.e820_table[1].type_, 2);

            assert_eq!(test_zero_page.e820_table[2].addr, 0x000F_0000);
            assert_eq!(test_zero_page.e820_table[2].size, 0);
            assert_eq!(test_zero_page.e820_table[2].type_, 2);

            assert_eq!(test_zero_page.e820_table[3].addr, 0x0010_0000);
            assert_eq!(test_zero_page.e820_table[3].size, 0x0ff0_0000);
            assert_eq!(test_zero_page.e820_table[3].type_, 1);
        }
    }
}
