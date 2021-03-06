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

//! # Micro VM
//!
//! Micro VM is a extremely light machine type.
//! It has a very simple machine model, which benefits to a very short
//! boot-time and tiny memory usage.
//!
//! ## Design
//!
//! This module offers support for:
//! 1. Create and manage lifecycle for `Micro VM`.
//! 2. Set cmdline arguments parameters for `Micro VM`.
//! 3. Manage mainloop to handle events for `Micro VM` and its devices.
//!
//! ## Platform Support
//!
//! - `x86_64`
//! - `aarch64`
extern crate address_space;
extern crate boot_loader;
extern crate machine_manager;
extern crate util;

pub mod cmdline;
pub mod main_loop;
pub mod micro_syscall;

use std::marker::{Send, Sync};
use std::ops::Deref;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::vec::Vec;

#[cfg(target_arch = "x86_64")]
use kvm_bindings::{kvm_pit_config, KVM_PIT_SPEAKER_DUMMY};
use kvm_ioctls::{Kvm, VmFd};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::terminal::Terminal;

#[cfg(target_arch = "x86_64")]
use address_space::KvmIoListener;
use address_space::{create_host_mmaps, AddressSpace, GuestAddress, KvmMemoryListener, Region};
use boot_loader::{load_kernel, BootLoaderConfig};
use machine_manager::config::{
    BootSource, ConsoleConfig, DriveConfig, NetworkInterfaceConfig, SerialConfig, VmConfig,
    VsockConfig,
};
use machine_manager::machine::{
    DeviceInterface, KvmVmState, MachineAddressInterface, MachineExternalInterface,
    MachineInterface, MachineLifecycle,
};
#[cfg(feature = "qmp")]
use machine_manager::{qmp, qmp::qmp_schema as schema, qmp::QmpChannel};
#[cfg(target_arch = "aarch64")]
use util::device_tree;
#[cfg(target_arch = "aarch64")]
use util::device_tree::CompileFDT;
use util::epoll_context::{
    EventNotifier, EventNotifierHelper, MainLoopManager, NotifierCallback, NotifierOperation,
};

use crate::cpu::{ArchCPU, CPUBootConfig, CPUInterface, CpuTopology, CPU};
use crate::errors::{Result, ResultExt};
#[cfg(target_arch = "aarch64")]
use crate::interrupt_controller::{InterruptController, InterruptControllerConfig};
#[cfg(target_arch = "aarch64")]
use crate::legacy::PL031;
#[cfg(target_arch = "aarch64")]
use crate::mmio::DeviceResource;
use crate::MainLoop;
use crate::{
    legacy::Serial,
    mmio::{Bus, DeviceType, VirtioMmioDevice},
    virtio::{vhost, Console},
};

/// Layout of aarch64
#[cfg(target_arch = "aarch64")]
pub const DRAM_BASE: u64 = 1 << 31;
#[cfg(target_arch = "aarch64")]
pub const MEM_MAPPED_IO_BASE: u64 = 1 << 30;

/// Layout of x86_64
#[cfg(target_arch = "x86_64")]
pub const MEM_MAPPED_IO_BASE: u64 = (1 << 32) - MEM_MAPPED_IO_SIZE;
#[cfg(target_arch = "x86_64")]
pub const MEM_MAPPED_IO_SIZE: u64 = 768 << 20;

/// Every type of devices depends on this configure-related trait to perform
/// initialization.
pub trait ConfigDevBuilder {
    /// Constructs device in `Bus` according configuration structure.
    ///
    /// # Arguments
    ///
    /// * `sys_mem` - The guest memory to device constructs over.
    /// * `bus` - The `mmio` bus where the device initializing.
    fn build_dev(&self, sys_mem: Arc<AddressSpace>, bus: &mut Bus) -> Result<()>;
}

impl ConfigDevBuilder for DriveConfig {
    fn build_dev(&self, _sys_mem: Arc<AddressSpace>, bus: &mut Bus) -> Result<()> {
        bus.fill_replaceable_device(&self.drive_id, Arc::new(self.clone()), DeviceType::BLK)
            .chain_err(|| "build dev from config failed")
    }
}

impl ConfigDevBuilder for NetworkInterfaceConfig {
    fn build_dev(&self, sys_mem: Arc<AddressSpace>, bus: &mut Bus) -> Result<()> {
        if self.vhost_type.is_some() {
            let net = Arc::new(Mutex::new(vhost::kernel::Net::new(
                self.clone(),
                sys_mem.clone(),
            )));
            let device = Arc::new(Mutex::new(VirtioMmioDevice::new(sys_mem, net)));
            bus.attach_device(device)
                .chain_err(|| "build dev from config failed")?;
            Ok(())
        } else {
            bus.fill_replaceable_device(&self.iface_id, Arc::new(self.clone()), DeviceType::NET)
                .chain_err(|| "build dev from config failed")
        }
    }
}

impl ConfigDevBuilder for ConsoleConfig {
    fn build_dev(&self, sys_mem: Arc<AddressSpace>, bus: &mut Bus) -> Result<()> {
        let console = Arc::new(Mutex::new(Console::new(self.clone())));
        let device = Arc::new(Mutex::new(VirtioMmioDevice::new(sys_mem, console)));
        bus.attach_device(device)
            .chain_err(|| "build dev from config failed")?;
        Ok(())
    }
}

impl ConfigDevBuilder for VsockConfig {
    fn build_dev(&self, sys_mem: Arc<AddressSpace>, bus: &mut Bus) -> Result<()> {
        let vsock = Arc::new(Mutex::new(vhost::kernel::Vsock::new(
            self.clone(),
            sys_mem.clone(),
        )));
        let device = Arc::new(Mutex::new(VirtioMmioDevice::new(sys_mem, vsock)));
        bus.attach_device(device)
            .chain_err(|| "build dev from config failed")?;
        Ok(())
    }
}

impl ConfigDevBuilder for SerialConfig {
    fn build_dev(&self, _sys_mem: Arc<AddressSpace>, bus: &mut Bus) -> Result<()> {
        let serial = Arc::new(Mutex::new(Serial::new()));
        bus.attach_device(serial.clone())
            .chain_err(|| "build dev from config failed")?;

        if self.stdio {
            MainLoop::update_event(EventNotifierHelper::internal_notifiers(serial))?;
        }
        Ok(())
    }
}

/// A wrapper around creating and using a kvm-based micro VM.
pub struct LightMachine {
    /// KVM VM file descriptor, represent VM entry in kvm module.
    vm_fd: Arc<VmFd>,
    /// `vCPU` topology, support sockets, cores, threads.
    cpu_topo: CpuTopology,
    /// `vCPU` devices.
    cpus: Arc<Mutex<Vec<Arc<CPU>>>>,
    /// Interrupt controller device.
    #[cfg(target_arch = "aarch64")]
    irq_chip: Arc<InterruptController>,
    /// Memory address space.
    sys_mem: Arc<AddressSpace>,
    /// IO address space.
    #[cfg(target_arch = "x86_64")]
    sys_io: Arc<AddressSpace>,
    /// Mmio bus.
    bus: Bus,
    /// VM running state.
    vm_state: Arc<(Mutex<KvmVmState>, Condvar)>,
    /// Vm boot_source config.
    boot_source: Arc<Mutex<BootSource>>,
    /// VM power button, handle VM `Shutdown` event.
    power_button: EventFd,
}

impl LightMachine {
    /// Constructs a new `LightMachine`.
    ///
    /// # Arguments
    ///
    /// * `vm_config` - Represents the configuration for VM.
    pub fn new(vm_config: VmConfig) -> Result<Arc<LightMachine>> {
        let kvm = Kvm::new().chain_err(|| "Failed to open /dev/kvm.")?;
        let vm_fd = Arc::new(
            kvm.create_vm()
                .chain_err(|| "KVM: failed to create VM fd failed")?,
        );

        let sys_mem = AddressSpace::new(Region::init_container_region(u64::max_value()))?;
        let nr_slots = kvm.get_nr_memslots();
        sys_mem.register_listener(Box::new(KvmMemoryListener::new(
            nr_slots as u32,
            vm_fd.clone(),
        )))?;

        #[cfg(target_arch = "x86_64")]
        let sys_io = AddressSpace::new(Region::init_container_region(1 << 16))?;
        #[cfg(target_arch = "x86_64")]
        sys_io.register_listener(Box::new(KvmIoListener::new(vm_fd.clone())))?;

        #[cfg(target_arch = "x86_64")]
        Self::arch_init(&vm_fd)?;

        // Init guest-memory
        // Define ram-region ranges according to architectures
        let ram_ranges = Self::arch_ram_ranges(vm_config.machine_config.mem_size);
        let mem_mappings = create_host_mmaps(&ram_ranges, vm_config.machine_config.omit_vm_memory)?;
        for mmap in mem_mappings.iter() {
            sys_mem.root().add_subregion(
                Region::init_ram_region(mmap.clone()),
                mmap.start_address().raw_value(),
            )?;
        }

        // Pre init vcpu and cpu topology
        let mut mask: Vec<u8> = Vec::with_capacity(vm_config.machine_config.nr_cpus as usize);
        for _i in 0..vm_config.machine_config.nr_cpus {
            mask.push(1)
        }

        let cpu_topo = CpuTopology {
            sockets: vm_config.machine_config.nr_cpus,
            cores: 1,
            threads: 1,
            nrcpus: vm_config.machine_config.nr_cpus,
            max_cpus: vm_config.machine_config.nr_cpus,
            online_mask: Arc::new(Mutex::new(mask)),
        };

        let nrcpus = vm_config.machine_config.nr_cpus;
        let mut vcpu_fds = vec![];
        for cpu_id in 0..nrcpus {
            vcpu_fds.push(Arc::new(vm_fd.create_vcpu(cpu_id)?));
        }

        // Interrupt Controller Chip init
        #[cfg(target_arch = "aarch64")]
        let intc_conf = InterruptControllerConfig {
            version: kvm_bindings::kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            map_region: 1 << 30,
            vcpu_count: u64::from(vm_config.machine_config.nr_cpus),
            max_irq: 192,
            msi: true,
        };
        #[cfg(target_arch = "aarch64")]
        let irq_chip = InterruptController::new(vm_fd.clone(), &intc_conf)?;

        // Machine state init
        let vm_state = Arc::new((Mutex::new(KvmVmState::Created), Condvar::new()));

        // Create vm object
        let mut vm = LightMachine {
            cpu_topo,
            cpus: Arc::new(Mutex::new(Vec::new())),
            #[cfg(target_arch = "aarch64")]
            irq_chip: Arc::new(irq_chip),
            sys_mem: sys_mem.clone(),
            #[cfg(target_arch = "x86_64")]
            sys_io,
            bus: Bus::new(sys_mem),
            boot_source: Arc::new(Mutex::new(vm_config.clone().boot_source)),
            vm_fd: vm_fd.clone(),
            vm_state,
            power_button: EventFd::new(libc::EFD_NONBLOCK)
                .chain_err(|| "Create EventFd for power-button failed.")?,
        };

        // Add mmio devices
        vm.add_devices(vm_config)?;

        let vm = Arc::new(vm);

        // Add vcpu object to vm
        let cpu_vm: Arc<Box<Arc<dyn MachineInterface + Send + Sync>>> =
            Arc::new(Box::new(vm.clone()));
        for vcpu_id in 0..nrcpus {
            #[cfg(target_arch = "aarch64")]
            let arch_cpu = ArchCPU::new(&vm_fd, u32::from(vcpu_id));

            #[cfg(target_arch = "x86_64")]
            let arch_cpu = ArchCPU::new(&vm_fd, u32::from(vcpu_id), u32::from(nrcpus));

            let cpu = CPU::new(
                vcpu_fds[vcpu_id as usize].clone(),
                vcpu_id,
                Arc::new(Mutex::new(arch_cpu)),
                cpu_vm.clone(),
            )?;

            let mut vcpus = vm.cpus.lock().unwrap();
            let newcpu = Arc::new(cpu);
            vcpus.push(newcpu.clone());
        }

        Ok(vm)
    }

    /// Calculate the ranges of memory according to architecture.
    ///
    /// # Arguments
    ///
    /// * `mem_size` - memory size of VM.
    ///
    /// # Returns
    ///
    /// A array of ranges, it's element represents (start_addr, size).
    /// On x86_64, there is a gap ranged from (4G - 768M) to 4G, which will be skipped.
    fn arch_ram_ranges(mem_size: u64) -> Vec<(u64, u64)> {
        // ranges is the vector of (start_addr, size)
        let mut ranges = Vec::<(u64, u64)>::new();

        #[cfg(target_arch = "aarch64")]
        ranges.push((DRAM_BASE, mem_size));

        #[cfg(target_arch = "x86_64")]
        {
            let gap_start = MEM_MAPPED_IO_BASE;
            ranges.push((0, std::cmp::min(gap_start, mem_size)));
            if mem_size > gap_start {
                let gap_end = MEM_MAPPED_IO_BASE + MEM_MAPPED_IO_SIZE;
                ranges.push((gap_end, mem_size - gap_start));
            }
        }

        ranges
    }

    #[cfg(target_arch = "x86_64")]
    fn arch_init(vm_fd: &VmFd) -> Result<()> {
        vm_fd.create_irq_chip()?;
        vm_fd.set_tss_address(0xfffb_d000 as usize)?;

        let mut pit_config = kvm_pit_config::default();
        pit_config.flags = KVM_PIT_SPEAKER_DUMMY;
        vm_fd.create_pit2(pit_config)?;

        Ok(())
    }

    /// Realize `LightMachine` means let all members of `LightMachine` enabled.
    #[cfg(target_arch = "aarch64")]
    pub fn realize(&self) -> Result<()> {
        self.bus
            .realize_devices(&self.vm_fd, &self.boot_source, &self.sys_mem)?;

        let boot_source = self.boot_source.lock().unwrap();

        let (initrd, initrd_size) = match &boot_source.initrd {
            Some(rd) => (Some(rd.initrd_file.clone()), rd.initrd_size),
            None => (None, 0),
        };

        let bootloader_config = BootLoaderConfig {
            kernel: boot_source.kernel_file.clone(),
            initrd,
            initrd_size: initrd_size as u32,
        };

        let layout = load_kernel(&bootloader_config, &self.sys_mem)?;
        if let Some(rd) = &boot_source.initrd {
            *rd.initrd_addr.lock().unwrap() = layout.initrd_start;
        }

        // need to release lock here, as generate_fdt_node will acquire it later
        drop(boot_source);

        let boot_config = CPUBootConfig {
            fdt_addr: layout.dtb_start,
            kernel_addr: layout.kernel_start,
        };

        for cpu_index in 0..self.cpu_topo.max_cpus {
            self.cpus.lock().unwrap()[cpu_index as usize].realize(&boot_config)?;
        }

        let mut fdt = vec![0; device_tree::FDT_MAX_SIZE as usize];
        self.generate_fdt_node(&mut fdt)?;

        self.sys_mem.write(
            &mut fdt.as_slice(),
            GuestAddress(boot_config.fdt_addr as u64),
            fdt.len() as u64,
        )?;

        self.register_power_event()?;

        Ok(())
    }

    /// Realize `LightMachine` means let all members of `LightMachine` enabled.
    #[cfg(target_arch = "x86_64")]
    pub fn realize(&self) -> Result<()> {
        self.bus.realize_devices(
            &self.vm_fd,
            &self.boot_source,
            &self.sys_mem,
            self.sys_io.clone(),
        )?;

        let boot_source = self.boot_source.lock().unwrap();

        // Load kernel image
        let (initrd, initrd_size) = match &boot_source.initrd {
            Some(rd) => (Some(rd.initrd_file.clone()), rd.initrd_size),
            None => (None, 0),
        };
        let bootloader_config = BootLoaderConfig {
            kernel: boot_source.kernel_file.clone(),
            initrd,
            initrd_size: initrd_size as u32,
            kernel_cmdline: boot_source.kernel_cmdline.to_string(),
            cpu_count: self.cpu_topo.nrcpus,
        };

        let layout = load_kernel(&bootloader_config, &self.sys_mem)?;
        let boot_config = CPUBootConfig {
            boot_ip: layout.kernel_start,
            boot_sp: layout.kernel_sp,
            zero_page: layout.zero_page_addr,
            code_segment: layout.segments.code_segment,
            data_segment: layout.segments.data_segment,
            gdt_base: layout.segments.gdt_base,
            gdt_size: layout.segments.gdt_limit,
            idt_base: layout.segments.idt_base,
            idt_size: layout.segments.idt_limit,
            pml4_start: layout.boot_pml4_addr,
        };

        for cpu_index in 0..self.cpu_topo.max_cpus {
            self.cpus.lock().unwrap()[cpu_index as usize].realize(&boot_config)?;
        }

        self.register_power_event()?;

        Ok(())
    }

    /// Start VM, changed `LightMachine`'s `vmstate` to `Paused` or
    /// `Running`.
    ///
    /// # Arguments
    ///
    /// * `paused` - After started, paused all vcpu or not.
    /// * `use_seccomp` - If use seccomp sandbox or not.
    pub fn vm_start(&self, paused: bool, use_seccomp: bool) -> Result<()> {
        let cpus_thread_barrier = Arc::new(Barrier::new((self.cpu_topo.max_cpus + 1) as usize));

        for cpu_index in 0..self.cpu_topo.max_cpus {
            let cpu_thread_barrier = cpus_thread_barrier.clone();
            let cpu = self.cpus.lock().unwrap()[cpu_index as usize].clone();
            CPU::start(cpu, cpu_thread_barrier, paused, use_seccomp)?;
        }

        let mut vmstate = self.vm_state.deref().0.lock().unwrap();
        if paused {
            *vmstate = KvmVmState::Paused;
        } else {
            *vmstate = KvmVmState::Running;
        }
        cpus_thread_barrier.wait();

        Ok(())
    }

    /// Pause VM, sleepy all vcpu thread. Changed `LightMachine`'s `vmstate`
    /// from `Running` to `Paused`.
    fn vm_pause(&self) -> Result<()> {
        for cpu_index in 0..self.cpu_topo.max_cpus {
            self.cpus.lock().unwrap()[cpu_index as usize].pause()?;
        }

        #[cfg(target_arch = "aarch64")]
        self.irq_chip.stop();

        let mut vmstate = self.vm_state.deref().0.lock().unwrap();
        *vmstate = KvmVmState::Paused;

        Ok(())
    }

    /// Resume VM, awaken all vcpu thread. Changed `LightMachine`'s `vmstate`
    /// from `Paused` to `Running`.
    fn vm_resume(&self) -> Result<()> {
        for cpu_index in 0..self.cpu_topo.max_cpus {
            self.cpus.lock().unwrap()[cpu_index as usize].resume()?;
        }

        let mut vmstate = self.vm_state.deref().0.lock().unwrap();
        *vmstate = KvmVmState::Running;

        Ok(())
    }

    /// Destroy VM, kill all vcpu thread. Changed `LightMachine`'s `vmstate`
    /// to `KVM_VMSTATE_DESTROY`.
    fn vm_destroy(&self) -> Result<()> {
        let mut vmstate = self.vm_state.deref().0.lock().unwrap();
        *vmstate = KvmVmState::Shutdown;

        let mut cpus = self.cpus.lock().unwrap();
        for cpu_index in 0..self.cpu_topo.max_cpus {
            cpus[cpu_index as usize].destroy()?;
        }
        cpus.clear();

        Ok(())
    }

    fn register_device<T: ConfigDevBuilder>(&mut self, dev_builder_ops: &T) -> Result<()> {
        dev_builder_ops.build_dev(self.sys_mem.clone(), &mut self.bus)
    }

    fn add_devices(&mut self, vm_config: VmConfig) -> Result<()> {
        #[cfg(target_arch = "aarch64")]
        {
            let rtc = Arc::new(Mutex::new(PL031::new()));
            self.bus
                .attach_device(rtc)
                .chain_err(|| "add rtc to bus failed")?;
        }

        if let Some(serial) = vm_config.serial {
            self.register_device(&serial)?;
        }

        if let Some(vsock) = vm_config.vsock {
            self.register_device(&vsock)?;
        }

        if let Some(drives) = vm_config.drives {
            for drive in drives {
                self.register_device(&drive)?;
            }
        }

        if let Some(nets) = vm_config.nets {
            for net in nets {
                self.register_device(&net)?;
            }
        }

        if let Some(consoles) = vm_config.consoles {
            for console in consoles {
                self.register_device(&console)?;
            }
        }

        Ok(())
    }

    fn register_power_event(&self) -> Result<()> {
        let power_button = self.power_button.try_clone().unwrap();
        let button_fd = power_button.as_raw_fd();
        let power_button_handler: Arc<Mutex<Box<NotifierCallback>>> =
            Arc::new(Mutex::new(Box::new(move |_, _| {
                let _ret = power_button.read().unwrap();
                None
            })));

        let notifier = EventNotifier::new(
            NotifierOperation::AddShared,
            button_fd,
            None,
            EventSet::IN,
            vec![power_button_handler],
        );

        MainLoop::update_event(vec![notifier])?;
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn generate_serial_device_node(
        &self,
        dev_info: &DeviceResource,
        fdt: &mut Vec<u8>,
    ) -> util::errors::Result<()> {
        let node = format!("/uart@{:x}", dev_info.addr);
        device_tree::add_sub_node(fdt, &node)?;
        device_tree::set_property_string(fdt, &node, "compatible", "ns16550a")?;
        device_tree::set_property_string(fdt, &node, "clock-names", "apb_pclk")?;
        device_tree::set_property_u32(fdt, &node, "clocks", device_tree::CLK_PHANDLE)?;
        device_tree::set_property_array_u64(fdt, &node, "reg", &[dev_info.addr, dev_info.size])?;
        device_tree::set_property_array_u32(
            fdt,
            &node,
            "interrupts",
            &[
                device_tree::GIC_FDT_IRQ_TYPE_SPI,
                dev_info.irq,
                device_tree::IRQ_TYPE_EDGE_RISING,
            ],
        )?;

        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn generate_rtc_device_node(
        &self,
        dev_info: &DeviceResource,
        fdt: &mut Vec<u8>,
    ) -> util::errors::Result<()> {
        let node = format!("/pl031@{:x}", dev_info.addr);
        device_tree::add_sub_node(fdt, &node)?;
        device_tree::set_property_string(fdt, &node, "compatible", "arm,pl031\0arm,primecell\0")?;
        device_tree::set_property_string(fdt, &node, "clock-names", "apb_pclk")?;
        device_tree::set_property_u32(fdt, &node, "clocks", device_tree::CLK_PHANDLE)?;
        device_tree::set_property_array_u64(fdt, &node, "reg", &[dev_info.addr, dev_info.size])?;
        device_tree::set_property_array_u32(
            fdt,
            &node,
            "interrupts",
            &[
                device_tree::GIC_FDT_IRQ_TYPE_SPI,
                dev_info.irq,
                device_tree::IRQ_TYPE_LEVEL_HIGH,
            ],
        )?;

        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn generate_virtio_devices_node(
        &self,
        dev_info: &DeviceResource,
        fdt: &mut Vec<u8>,
    ) -> util::errors::Result<()> {
        let node = format!("/virtio_mmio@{:x}", dev_info.addr);
        device_tree::add_sub_node(fdt, &node)?;
        device_tree::set_property_string(fdt, &node, "compatible", "virtio,mmio")?;
        device_tree::set_property_u32(fdt, &node, "interrupt-parent", device_tree::GIC_PHANDLE)?;
        device_tree::set_property_array_u64(fdt, &node, "reg", &[dev_info.addr, dev_info.size])?;
        device_tree::set_property_array_u32(
            fdt,
            &node,
            "interrupts",
            &[
                device_tree::GIC_FDT_IRQ_TYPE_SPI,
                dev_info.irq,
                device_tree::IRQ_TYPE_EDGE_RISING,
            ],
        )?;

        Ok(())
    }
}

impl MachineLifecycle for LightMachine {
    fn pause(&self) -> bool {
        if self.notify_lifecycle(KvmVmState::Running, KvmVmState::Paused) {
            #[cfg(feature = "qmp")]
            event!(STOP);

            true
        } else {
            false
        }
    }

    fn resume(&self) -> bool {
        if !self.notify_lifecycle(KvmVmState::Paused, KvmVmState::Running) {
            return false;
        }

        #[cfg(feature = "qmp")]
        event!(RESUME);

        true
    }

    fn destroy(&self) -> bool {
        let vmstate = {
            let state = self.vm_state.deref().0.lock().unwrap();
            *state
        };

        if !self.notify_lifecycle(vmstate, KvmVmState::Shutdown) {
            return false;
        }

        true
    }

    fn notify_lifecycle(&self, old: KvmVmState, new: KvmVmState) -> bool {
        use KvmVmState::*;

        let vmstate = self.vm_state.deref().0.lock().unwrap();
        if *vmstate != old {
            error!("Vm lifecycle error: state check failed.");
            return false;
        }
        drop(vmstate);

        match (old, new) {
            (Created, Running) => {
                if let Err(e) = self.vm_start(false, false) {
                    error!("Vm lifecycle error:{}", e);
                };
            }
            (Running, Paused) => {
                if let Err(e) = self.vm_pause() {
                    error!("Vm lifecycle error:{}", e);
                };
            }
            (Paused, Running) => {
                if let Err(e) = self.vm_resume() {
                    error!("Vm lifecycle error:{}", e);
                };
            }
            (_, Shutdown) => {
                if let Err(e) = self.vm_destroy() {
                    error!("Vm lifecycle error:{}", e);
                };
                self.power_button.write(1).unwrap();
            }
            (_, _) => {
                error!("Vm lifecycle error: this transform is illegal.");
                return false;
            }
        }

        let vmstate = self.vm_state.deref().0.lock().unwrap();
        if *vmstate != new {
            error!("Vm lifecycle error: state transform failed.");
            return false;
        }

        true
    }
}

impl MachineAddressInterface for LightMachine {
    #[cfg(target_arch = "x86_64")]
    fn pio_in(&self, addr: u64, mut data: &mut [u8]) -> bool {
        // The function pit_calibrate_tsc() in kernel gets stuck if data read from
        // io-port 0x61 is not 0x20.
        // This problem only happens before Linux version 4.18 (fixed by 368a540e0)
        if addr == 0x61 {
            data[0] = 0x20;
            return true;
        }
        let length = data.len() as u64;
        self.sys_io
            .read(&mut data, GuestAddress(addr), length)
            .is_ok()
    }

    #[cfg(target_arch = "x86_64")]
    fn pio_out(&self, addr: u64, mut data: &[u8]) -> bool {
        let count = data.len() as u64;
        self.sys_io
            .write(&mut data, GuestAddress(addr), count)
            .is_ok()
    }

    fn mmio_read(&self, addr: u64, mut data: &mut [u8]) -> bool {
        let length = data.len() as u64;
        self.sys_mem
            .read(&mut data, GuestAddress(addr), length)
            .is_ok()
    }

    fn mmio_write(&self, addr: u64, mut data: &[u8]) -> bool {
        let count = data.len() as u64;
        self.sys_mem
            .write(&mut data, GuestAddress(addr), count)
            .is_ok()
    }
}

impl DeviceInterface for LightMachine {
    #[cfg(feature = "qmp")]
    fn query_status(&self) -> qmp::Response {
        let vmstate = self.vm_state.deref().0.lock().unwrap();
        let qmp_state = match *vmstate {
            KvmVmState::Running => schema::StatusInfo {
                singlestep: false,
                running: true,
                status: schema::RunState::running,
            },
            KvmVmState::Paused => schema::StatusInfo {
                singlestep: false,
                running: true,
                status: schema::RunState::paused,
            },
            _ => Default::default(),
        };

        qmp::Response::create_response(serde_json::to_value(&qmp_state).unwrap(), None)
    }

    #[cfg(feature = "qmp")]
    fn query_cpus(&self) -> qmp::Response {
        let mut cpu_vec: Vec<serde_json::Value> = Vec::new();
        for cpu_index in 0..self.cpu_topo.max_cpus {
            if self.cpu_topo.get_mask(cpu_index as usize) == 1 {
                let thread_id = self.cpus.lock().unwrap()[cpu_index as usize].tid();
                let (socketid, coreid, threadid) = self.cpu_topo.get_topo(cpu_index as usize);
                let cpu_instance = schema::CpuInstanceProperties {
                    node_id: None,
                    socket_id: Some(socketid as isize),
                    core_id: Some(coreid as isize),
                    thread_id: Some(threadid as isize),
                };
                #[cfg(target_arch = "x86_64")]
                {
                    let cpu_info = schema::CpuInfo::x86 {
                        current: true,
                        qom_path: String::from("/machine/unattached/device[")
                            + &cpu_index.to_string()
                            + &"]".to_string(),
                        halted: false,
                        props: Some(cpu_instance),
                        CPU: cpu_index as isize,
                        thread_id: thread_id as isize,
                        x86: schema::CpuInfoX86 {},
                    };
                    cpu_vec.push(serde_json::to_value(cpu_info).unwrap());
                }
                #[cfg(target_arch = "aarch64")]
                {
                    let cpu_info = schema::CpuInfo::Arm {
                        current: true,
                        qom_path: String::from("/machine/unattached/device[")
                            + &cpu_index.to_string()
                            + &"]".to_string(),
                        halted: false,
                        props: Some(cpu_instance),
                        CPU: cpu_index as isize,
                        thread_id: thread_id as isize,
                        arm: schema::CpuInfoArm {},
                    };
                    cpu_vec.push(serde_json::to_value(cpu_info).unwrap());
                }
            }
        }
        qmp::Response::create_response(cpu_vec.into(), None)
    }

    #[cfg(feature = "qmp")]
    fn query_hotpluggable_cpus(&self) -> qmp::Response {
        let mut hotplug_vec: Vec<serde_json::Value> = Vec::new();
        #[cfg(target_arch = "x86_64")]
        let cpu_type = String::from("host-x86-cpu");
        #[cfg(target_arch = "aarch64")]
        let cpu_type = String::from("host-aarch64-cpu");

        for cpu_index in 0..self.cpu_topo.max_cpus {
            if self.cpu_topo.get_mask(cpu_index as usize) == 0 {
                let (socketid, coreid, threadid) = self.cpu_topo.get_topo(cpu_index as usize);
                let cpu_instance = schema::CpuInstanceProperties {
                    node_id: None,
                    socket_id: Some(socketid as isize),
                    core_id: Some(coreid as isize),
                    thread_id: Some(threadid as isize),
                };
                let hotpluggable_cpu = schema::HotpluggableCPU {
                    type_: cpu_type.clone(),
                    vcpus_count: 1,
                    props: cpu_instance,
                    qom_path: None,
                };
                hotplug_vec.push(serde_json::to_value(hotpluggable_cpu).unwrap());
            } else {
                let (socketid, coreid, threadid) = self.cpu_topo.get_topo(cpu_index as usize);
                let cpu_instance = schema::CpuInstanceProperties {
                    node_id: None,
                    socket_id: Some(socketid as isize),
                    core_id: Some(coreid as isize),
                    thread_id: Some(threadid as isize),
                };
                let hotpluggable_cpu = schema::HotpluggableCPU {
                    type_: cpu_type.clone(),
                    vcpus_count: 1,
                    props: cpu_instance,
                    qom_path: Some(
                        String::from("/machine/unattached/device[")
                            + &cpu_index.to_string()
                            + &"]".to_string(),
                    ),
                };
                hotplug_vec.push(serde_json::to_value(hotpluggable_cpu).unwrap());
            }
        }
        qmp::Response::create_response(hotplug_vec.into(), None)
    }

    fn device_add(
        &self,
        id: String,
        driver: String,
        addr: Option<String>,
        lun: Option<usize>,
    ) -> bool {
        // get slot of bus by addr or lun
        let mut slot = 0;
        if let Some(addr) = addr {
            let slot_str = addr.as_str().trim_start_matches("0x");

            if let Ok(n) = usize::from_str_radix(slot_str, 16) {
                slot = n;
            }
        } else if let Some(lun) = lun {
            slot = lun + 1;
        }

        self.bus.add_replaceable_device(&id, &driver, slot).is_ok()
    }

    fn device_del(&self, device_id: String) -> bool {
        match self.bus.del_replaceable_device(&device_id) {
            Ok(path) => {
                #[cfg(feature = "qmp")]
                {
                    let block_del_event = schema::DEVICE_DELETED {
                        device: Some(device_id),
                        path,
                    };
                    event!(DEVICE_DELETED; block_del_event);
                }

                true
            }
            _ => false,
        }
    }

    fn blockdev_add(
        &self,
        node_name: String,
        file: schema::FileOptions,
        cache: Option<schema::CacheOptions>,
        read_only: Option<bool>,
    ) -> bool {
        let read_only = if let Some(ro) = read_only { ro } else { false };

        let direct = if let Some(cache) = cache {
            match cache.direct {
                Some(direct) => direct,
                _ => true,
            }
        } else {
            true
        };

        let config = DriveConfig {
            drive_id: node_name.clone(),
            path_on_host: file.filename,
            read_only,
            direct,
            serial_num: None,
        };

        self.bus
            .add_replaceable_config(node_name, Arc::new(config))
            .is_ok()
    }

    fn netdev_add(&self, id: String, if_name: Option<String>, fds: Option<String>) -> bool {
        let mut config = NetworkInterfaceConfig {
            iface_id: id.clone(),
            host_dev_name: "".to_string(),
            mac: None,
            tap_fd: None,
            vhost_type: None,
            vhost_fd: None,
        };

        if let Some(fds) = fds {
            let netdev_fd = if fds.contains(':') {
                let col: Vec<_> = fds.split(':').collect();
                String::from(col[col.len() - 1])
            } else {
                String::from(&fds)
            };

            #[cfg(feature = "qmp")]
            {
                if let Some(fd_num) = QmpChannel::get_fd(&netdev_fd) {
                    config.tap_fd = Some(fd_num);
                } else {
                    // try to convert string to RawFd
                    let fd_num = match netdev_fd.parse::<i32>() {
                        Ok(fd) => fd,
                        _ => {
                            error!(
                                "Add netdev error: failed to convert {} to RawFd.",
                                netdev_fd
                            );
                            return false;
                        }
                    };

                    config.tap_fd = Some(fd_num);
                }
            }
        } else if let Some(if_name) = if_name {
            config.host_dev_name = if_name;
        }

        self.bus
            .add_replaceable_config(id, Arc::new(config))
            .is_ok()
    }

    #[cfg(feature = "qmp")]
    fn getfd(&self, fd_name: String, if_fd: Option<RawFd>) -> qmp::Response {
        if let Some(fd) = if_fd {
            QmpChannel::set_fd(fd_name, fd);
            qmp::Response::create_empty_response()
        } else {
            let err_resp = schema::QmpErrorClass::GenericError("Invalid SCM message".to_string());
            qmp::Response::create_error_response(err_resp, None).unwrap()
        }
    }
}

impl MachineInterface for LightMachine {}
impl MachineExternalInterface for LightMachine {}

impl MainLoopManager for LightMachine {
    fn main_loop_should_exit(&self) -> bool {
        let vmstate = self.vm_state.deref().0.lock().unwrap();
        *vmstate == KvmVmState::Shutdown
    }

    fn main_loop_cleanup(&self) -> util::errors::Result<()> {
        if let Err(e) = std::io::stdin().lock().set_canon_mode() {
            error!(
                "destroy virtual machine: reset stdin to canonical mode failed, {}",
                e
            );
        }

        Ok(())
    }
}

#[cfg(target_arch = "aarch64")]
trait CompileFDTHelper {
    fn generate_cpu_nodes(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()>;
    fn generate_memory_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()>;
    fn generate_devices_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()>;
    fn generate_chosen_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()>;
}

#[cfg(target_arch = "aarch64")]
impl CompileFDTHelper for LightMachine {
    fn generate_cpu_nodes(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()> {
        let node = "/cpus";

        device_tree::add_sub_node(fdt, node)?;
        device_tree::set_property_u32(fdt, node, "#address-cells", 0x02)?;
        device_tree::set_property_u32(fdt, node, "#size-cells", 0x0)?;

        // Generate CPU topology
        if self.cpu_topo.max_cpus > 0 && self.cpu_topo.max_cpus % 8 == 0 {
            device_tree::add_sub_node(fdt, "/cpus/cpu-map")?;

            let sockets = self.cpu_topo.max_cpus / 8;
            for cluster in 0..u32::from(sockets) {
                let clster = format!("/cpus/cpu-map/cluster{}", cluster);
                device_tree::add_sub_node(fdt, &clster)?;

                for i in 0..2 as u32 {
                    let sub_cluster = format!("{}/cluster{}", clster, i);
                    device_tree::add_sub_node(fdt, &sub_cluster)?;

                    let core0 = format!("{}/core0", sub_cluster);
                    device_tree::add_sub_node(fdt, &core0)?;
                    let thread0 = format!("{}/thread0", core0);
                    device_tree::add_sub_node(fdt, &thread0)?;
                    device_tree::set_property_u32(fdt, &thread0, "cpu", cluster * 8 + i * 4 + 10)?;

                    let thread1 = format!("{}/thread1", core0);
                    device_tree::add_sub_node(fdt, &thread1)?;
                    device_tree::set_property_u32(
                        fdt,
                        &thread1,
                        "cpu",
                        cluster * 8 + i * 4 + 10 + 1,
                    )?;

                    let core1 = format!("{}/core1", sub_cluster);
                    device_tree::add_sub_node(fdt, &core1)?;
                    let thread0 = format!("{}/thread0", core1);
                    device_tree::add_sub_node(fdt, &thread0)?;
                    device_tree::set_property_u32(
                        fdt,
                        &thread0,
                        "cpu",
                        cluster * 8 + i * 4 + 10 + 2,
                    )?;

                    let thread1 = format!("{}/thread1", core1);
                    device_tree::add_sub_node(fdt, &thread1)?;
                    device_tree::set_property_u32(
                        fdt,
                        &thread1,
                        "cpu",
                        cluster * 8 + i * 4 + 10 + 3,
                    )?;
                }
            }
        }

        let cpu_list = self.cpus.lock().unwrap();
        for cpu_index in 0..self.cpu_topo.max_cpus {
            let mpidr = cpu_list[cpu_index as usize]
                .arch()
                .lock()
                .unwrap()
                .get_mpidr(cpu_list[cpu_index as usize].fd());

            let node = format!("/cpus/cpu@{:x}", mpidr);
            device_tree::add_sub_node(fdt, &node)?;
            device_tree::set_property_u32(
                fdt,
                &node,
                "phandle",
                u32::from(cpu_index) + device_tree::CPU_PHANDLE_START,
            )?;
            device_tree::set_property_string(fdt, &node, "device_type", "cpu")?;
            device_tree::set_property_string(fdt, &node, "compatible", "arm,arm-v8")?;
            if self.cpu_topo.max_cpus > 1 {
                device_tree::set_property_string(fdt, &node, "enable-method", "psci")?;
            }
            device_tree::set_property_u64(fdt, &node, "reg", mpidr & 0x007F_FFFF)?;
        }

        Ok(())
    }

    fn generate_memory_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()> {
        let mem_size = self.sys_mem.memory_end_address().raw_value() - 0x8000_0000;
        let node = "/memory";
        device_tree::add_sub_node(fdt, node)?;
        device_tree::set_property_string(fdt, node, "device_type", "memory")?;
        device_tree::set_property_array_u64(fdt, node, "reg", &[0x8000_0000, mem_size as u64])?;

        Ok(())
    }

    fn generate_devices_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()> {
        // timer
        let mut cells: Vec<u32> = Vec::new();
        for &irq in [13, 14, 11, 10].iter() {
            cells.push(device_tree::GIC_FDT_IRQ_TYPE_PPI);
            cells.push(irq);
            cells.push(device_tree::IRQ_TYPE_LEVEL_HIGH);
        }
        let node = "/timer";
        device_tree::add_sub_node(fdt, node)?;
        device_tree::set_property_string(fdt, node, "compatible", "arm,armv8-timer")?;
        device_tree::set_property(fdt, node, "always-on", None)?;
        device_tree::set_property_array_u32(fdt, node, "interrupts", &cells)?;

        // clock
        let node = "/apb-pclk";
        device_tree::add_sub_node(fdt, node)?;
        device_tree::set_property_string(fdt, node, "compatible", "fixed-clock")?;
        device_tree::set_property_string(fdt, node, "clock-output-names", "clk24mhz")?;
        device_tree::set_property_u32(fdt, node, "#clock-cells", 0x0)?;
        device_tree::set_property_u32(fdt, node, "clock-frequency", 24_000_000)?;
        device_tree::set_property_u32(fdt, node, "phandle", device_tree::CLK_PHANDLE)?;

        // psci
        let node = "/psci";
        device_tree::add_sub_node(fdt, node)?;
        device_tree::set_property_string(fdt, node, "compatible", "arm,psci-0.2")?;
        device_tree::set_property_string(fdt, node, "method", "hvc")?;

        for dev_info in self.bus.get_devices_info().iter().rev() {
            match dev_info.dev_type {
                DeviceType::SERIAL => {
                    self.generate_serial_device_node(dev_info, fdt)?;
                }
                DeviceType::RTC => {
                    self.generate_rtc_device_node(dev_info, fdt)?;
                }
                _ => {
                    self.generate_virtio_devices_node(dev_info, fdt)?;
                }
            }
        }

        Ok(())
    }

    fn generate_chosen_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()> {
        let node = "/chosen";

        let boot_source = self.boot_source.lock().unwrap();

        device_tree::add_sub_node(fdt, node)?;
        let cmdline = &boot_source.kernel_cmdline.to_string();
        device_tree::set_property_string(fdt, node, "bootargs", cmdline.as_str())?;

        match &boot_source.initrd {
            Some(initrd) => {
                device_tree::set_property_u64(
                    fdt,
                    node,
                    "linux,initrd-start",
                    *initrd.initrd_addr.lock().unwrap(),
                )?;
                device_tree::set_property_u64(
                    fdt,
                    node,
                    "linux,initrd-end",
                    *initrd.initrd_addr.lock().unwrap() + initrd.initrd_size,
                )?;
            }
            None => {}
        }

        Ok(())
    }
}

#[cfg(target_arch = "aarch64")]
impl device_tree::CompileFDT for LightMachine {
    fn generate_fdt_node(&self, fdt: &mut Vec<u8>) -> util::errors::Result<()> {
        device_tree::create_device_tree(fdt)?;

        device_tree::set_property_string(fdt, "/", "compatible", "linux,dummy-virt")?;
        device_tree::set_property_u32(fdt, "/", "#address-cells", 0x2)?;
        device_tree::set_property_u32(fdt, "/", "#size-cells", 0x2)?;
        device_tree::set_property_u32(fdt, "/", "interrupt-parent", device_tree::GIC_PHANDLE)?;

        self.generate_cpu_nodes(fdt)?;
        self.generate_memory_node(fdt)?;
        self.generate_devices_node(fdt)?;
        self.generate_chosen_node(fdt)?;
        self.irq_chip.generate_fdt_node(fdt)?;

        Ok(())
    }
}
