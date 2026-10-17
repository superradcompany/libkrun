//! Linux PID 1 that keeps a real KVM child executing across an outer VM snapshot.
use kvm_bindings::{
    kvm_irqchip, kvm_pit_config, kvm_regs, kvm_userspace_memory_region, KVM_IRQCHIP_PIC_MASTER,
};
use kvm_ioctls::{Kvm, VcpuExit};
use std::{
    fs::File,
    io::{self, Read, Write},
    thread,
    time::Duration,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() {
    if let Err(error) = run() {
        eprintln!("nested counter guest failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // PID 1 needs /dev/kvm and the kernel random device before constructing the VM.
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
    vm.create_irq_chip()?;
    vm.create_pit2(kvm_pit_config::default())?;

    // Anonymous, page-aligned RAM remains mapped until the process exits. It is
    // shared with KVM, so read progress through raw volatile loads, not references.
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
    // Real-mode PIC/PIT setup, followed by an increment loop with IRQs enabled.
    let code: &[u8] = &[
        0xfa, 0xb0, 0x11, 0xe6, 0x20, 0xb0, 0x08, 0xe6, 0x21, 0xb0, 0x04, 0xe6, 0x21, 0xb0, 0x01,
        0xe6, 0x21, 0xb0, 0xfe, 0xe6, 0x21, 0xb0, 0x36, 0xe6, 0x43, 0xb0, 0xff, 0xe6, 0x40, 0xe6,
        0x40, 0xfb, 0x9c, 0x58, 0xa3, 0x08, 0x08, 0x66, 0xff, 0x06, 0x00, 0x08, 0xeb, 0xf4,
    ];
    let interrupt: &[u8] = &[
        0x50, 0x66, 0xff, 0x06, 0x04, 0x08, 0xb0, 0x20, 0xe6, 0x20, 0x58, 0xcf,
    ];
    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr(), memory.add(0x1000), code.len());
        std::ptr::copy_nonoverlapping(interrupt.as_ptr(), memory.add(0x300), interrupt.len());
        memory.add(8 * 4).write(0);
        memory.add(8 * 4 + 1).write(3);
        vm.set_user_memory_region(kvm_userspace_memory_region {
            slot: 0,
            guest_phys_addr: 0,
            memory_size: 0x10000,
            userspace_addr: memory as u64,
            flags: 0,
        })?;
    }
    let mut cpu = vm.create_vcpu(0)?;
    let mut segments = cpu.get_sregs()?;
    segments.cs.base = 0;
    segments.cs.selector = 0;
    cpu.set_sregs(&segments)?;
    cpu.set_regs(&kvm_regs {
        rip: 0x1000,
        rsp: 0x7000,
        rflags: 2,
        ..Default::default()
    })?;
    thread::spawn(move || loop {
        match cpu.run() {
            Ok(VcpuExit::Intr) => {}
            Err(error) if error.errno() == libc::EINTR => {}
            result => {
                eprintln!("unexpected inner exit: {result:?}");
                std::process::exit(1);
            }
        }
    });

    let counters = unsafe { memory.add(0x800).cast::<u32>() };
    loop {
        let mut pic = kvm_irqchip {
            chip_id: KVM_IRQCHIP_PIC_MASTER,
            ..Default::default()
        };
        vm.get_irqchip(&mut pic)?;
        // KVM owns writes to these words. They are aligned and never accessed
        // through a Rust reference while the inner CPU runs.
        let (count, ticks, flags, pic) = unsafe {
            (
                counters.read_volatile(),
                counters.add(1).read_volatile(),
                counters.add(2).read_volatile() & 0xffff,
                pic.chip.pic,
            )
        };
        println!(
            "NESTED_PROGRESS {count} {ticks} {identity} flags={flags:x} irr={:x} isr={:x} imr={:x}",
            pic.irr, pic.isr, pic.imr
        );
        io::stdout().flush()?;
        thread::sleep(Duration::from_millis(100));
    }
}
