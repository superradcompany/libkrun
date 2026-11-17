//! PID 1 in the inner Linux VM. Its disk and RAM must survive outer-VM restore.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn mount_fs(source: &str, target: &str, kind: &str) -> io::Result<()> {
    match fs::create_dir(target) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    let (source, target, kind) = (
        CString::new(source)?,
        CString::new(target)?,
        CString::new(kind)?,
    );
    // All strings are NUL terminated and remain alive for the syscall.
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            kind.as_ptr(),
            0,
            ptr::null(),
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EBUSY) {
            return Err(error);
        }
    }
    Ok(())
}

fn record(identity: u64, generation: u64) -> [u8; 24] {
    let mut bytes = [0; 24];
    bytes[..8].copy_from_slice(&identity.to_ne_bytes());
    bytes[8..16].copy_from_slice(&generation.to_ne_bytes());
    bytes[16..].copy_from_slice(&(identity ^ generation).to_ne_bytes());
    bytes
}

fn evict(file: &File) -> io::Result<()> {
    // posix_fadvise returns an error number rather than setting errno.
    let error = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    Ok(())
}

fn busy_workers() -> io::Result<Option<&'static [AtomicU64; 2]>> {
    if std::env::var("NESTED_BUSY").as_deref() != Ok("1") {
        return Ok(None);
    }
    // The mapping is shared across fork and intentionally lives until PID 1 exits.
    let mapping = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size_of::<[AtomicU64; 2]>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mapping == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let counters = mapping.cast::<[AtomicU64; 2]>();
    // mmap provides sufficient alignment; initialize before publishing to children.
    unsafe { counters.write([AtomicU64::new(0), AtomicU64::new(0)]) };
    let counters = unsafe { &*counters };
    for counter in counters {
        // This process is single threaded; children only perform atomic operations.
        match unsafe { libc::fork() } {
            -1 => return Err(io::Error::last_os_error()),
            0 => loop {
                counter.fetch_add(1, Ordering::Relaxed);
            },
            _ => {}
        }
    }
    println!("NESTED_BUSY workers=2");
    Ok(Some(counters))
}

fn run() -> io::Result<()> {
    mount_fs("devtmpfs", "/dev", "devtmpfs")?;
    mount_fs("proc", "/proc", "proc")?;
    mount_fs("/dev/vda", "/data", "ext4")?;

    let mut identity = 0u64;
    // The writable buffer is exactly one u64; a short random read fails closed.
    if unsafe { libc::getrandom((&mut identity as *mut u64).cast(), size_of::<u64>(), 0) } != 8 {
        return Err(io::Error::other(
            "getrandom did not return a complete identity",
        ));
    }
    // A reboot against the old disk must fail instead of masquerading as restore.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open("/data/progress")?;
    let mut generation = 0u64;
    let mut saved = record(identity, generation);
    file.write_all_at(&saved, 0)?;
    file.sync_all()?;
    File::open("/data")?.sync_all()?;
    let busy = busy_workers()?;

    loop {
        // Compare disk with captured RAM before overwriting the restored record.
        evict(&file)?;
        let mut actual = [0; 24];
        file.read_exact_at(&mut actual, 0)?;
        if actual != saved {
            return Err(io::Error::other(
                "disk record disagrees with running guest memory",
            ));
        }
        generation += 1;
        saved = record(identity, generation);
        file.write_all_at(&saved, 0)?;
        file.sync_all()?;
        // Evict clean pages so readback exercises the virtual block device.
        evict(&file)?;
        file.read_exact_at(&mut actual, 0)?;
        if actual != saved {
            return Err(io::Error::other(
                "committed disk record could not be read back",
            ));
        }
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let elapsed_ms = (now.tv_sec as u64 * 1000 + now.tv_nsec as u64 / 1_000_000) as u32;
        if let Some(counters) = busy {
            println!(
                "NESTED_BUSY_PROGRESS {} {}",
                counters[0].load(Ordering::Relaxed),
                counters[1].load(Ordering::Relaxed)
            );
        }
        let disk_generation = u64::from_ne_bytes(actual[8..16].try_into().unwrap());
        println!(
            "NESTED_PROGRESS {} {} {} disk={}",
            generation as u32, elapsed_ms, identity, disk_generation
        );
        io::stdout().flush()?;
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("FAIL: {error}");
        std::process::exit(1);
    }
}
