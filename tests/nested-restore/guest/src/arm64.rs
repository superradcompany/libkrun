//! Linux KVM parent for the ARM64 nested counter and virtual timer workload.
use std::{
    arch::global_asm,
    fs::File,
    io::{self, Read, Write},
    mem::offset_of,
    thread,
    time::Duration,
};

use kvm_bindings::{
    kvm_create_device, kvm_device_attr, kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3, kvm_regs,
    kvm_userspace_memory_region, kvm_vcpu_init, user_pt_regs, KVM_DEV_ARM_VGIC_CTRL_INIT,
    KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_DEV_ARM_VGIC_GRP_CTRL, KVM_REG_ARM64, KVM_REG_ARM_CORE,
    KVM_REG_SIZE_U64, KVM_VGIC_V3_ADDR_TYPE_DIST, KVM_VGIC_V3_ADDR_TYPE_REDIST,
};
use kvm_ioctls::{DeviceFd, Kvm, VcpuExit};

global_asm!(include_str!("../../inner-arm64.S"));

extern "C" {
    static inner_start: u8;
    static inner_end: u8;
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn main() {
    if let Err(error) = run() {
        eprintln!("nested counter guest failed: {error}");
        std::process::exit(1);
    }
}

fn attr(device: &DeviceFd, group: u32, attr: u64, value: Option<&u64>) -> Result<()> {
    device.set_device_attr(&kvm_device_attr {
        group,
        attr,
        addr: value.map_or(0, |value| value as *const u64 as u64),
        flags: 0,
    })?;
    Ok(())
}

fn run() -> Result<()> {
    let mounted = unsafe {
        libc::mount(
            c"devtmpfs".as_ptr(),
            c"/dev".as_ptr(),
            c"devtmpfs".as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if mounted < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EBUSY) {
        return Err(io::Error::last_os_error().into());
    }
    let mut random = [0; 8];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    let identity = u64::from_ne_bytes(random);
    let kvm = Kvm::new()?;
    let vm = kvm.create_vm()?;

    // KVM owns accesses to this RAM until process exit. Never alias its progress
    // words through Rust references while the child runs.
    let memory = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            0x10000,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if memory == libc::MAP_FAILED {
        return Err(io::Error::last_os_error().into());
    }
    let memory = memory.cast::<u8>();
    let start = std::ptr::addr_of!(inner_start);
    let length = std::ptr::addr_of!(inner_end) as usize - start as usize;
    if length > 0x4000 {
        return Err("inner payload overlaps progress words".into());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(start, memory, length);
        vm.set_user_memory_region(kvm_userspace_memory_region {
            slot: 0,
            guest_phys_addr: 0x40000000,
            memory_size: 0x10000,
            userspace_addr: memory as u64,
            flags: 0,
        })?;
    }

    let mut cpu = vm.create_vcpu(0)?;
    let mut init = kvm_vcpu_init::default();
    vm.get_preferred_target(&mut init)?;
    cpu.vcpu_init(&init)?;

    let gic = vm.create_device(&mut kvm_create_device {
        type_: kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
        ..Default::default()
    })?;
    attr(
        &gic,
        KVM_DEV_ARM_VGIC_GRP_ADDR,
        KVM_VGIC_V3_ADDR_TYPE_DIST.into(),
        Some(&0x08000000),
    )?;
    attr(
        &gic,
        KVM_DEV_ARM_VGIC_GRP_ADDR,
        KVM_VGIC_V3_ADDR_TYPE_REDIST.into(),
        Some(&0x080a0000),
    )?;
    attr(
        &gic,
        KVM_DEV_ARM_VGIC_GRP_CTRL,
        KVM_DEV_ARM_VGIC_CTRL_INIT.into(),
        None,
    )?;

    // ARM core register IDs encode the byte offset in units of u32.
    let core = KVM_REG_ARM64 | KVM_REG_SIZE_U64 | u64::from(KVM_REG_ARM_CORE);
    for (offset, value) in [
        (
            offset_of!(kvm_regs, regs) + offset_of!(user_pt_regs, pc),
            0x40000000_u64,
        ),
        (
            offset_of!(kvm_regs, regs) + offset_of!(user_pt_regs, pstate),
            0x3c5,
        ),
        (offset_of!(kvm_regs, sp_el1), 0x4000f000),
    ] {
        cpu.set_one_reg(core | (offset / 4) as u64, &value.to_ne_bytes())?;
    }

    thread::spawn(move || loop {
        match cpu.run() {
            Ok(VcpuExit::Intr) => {}
            Err(error) if error.errno() == libc::EINTR => {}
            result => {
                eprintln!("unexpected inner exit: {result:?}");
                if matches!(result, Ok(VcpuExit::InternalError)) {
                    let internal = unsafe { cpu.get_kvm_run().__bindgen_anon_1.internal };
                    let count = (internal.ndata as usize).min(internal.data.len());
                    eprintln!(
                        "internal suberror={} ndata={} data={:x?}",
                        internal.suberror,
                        internal.ndata,
                        &internal.data[..count]
                    );
                }
                let mut messages = [0_u8; 16384];
                let length = unsafe {
                    libc::klogctl(3, messages.as_mut_ptr().cast(), messages.len() as i32)
                };
                if length > 0 {
                    eprintln!("{}", String::from_utf8_lossy(&messages[..length as usize]));
                }
                std::process::exit(1);
            }
        }
    });

    let counters = unsafe { memory.add(0x4000).cast::<u32>() };
    loop {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let (count, ticks, fatal) = unsafe {
            (
                counters.read_volatile(),
                counters.add(1).read_volatile(),
                counters.add(2).read_volatile(),
            )
        };
        if fatal != 0 {
            return Err("inner guest took a fatal exception".into());
        }
        let monotonic_ns = now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64;
        println!(
            "NESTED_PROGRESS {count} {ticks} {identity} fatal={fatal} monotonic_ns={monotonic_ns}"
        );
        io::stdout().flush()?;
        thread::sleep(Duration::from_millis(100));
    }
}
