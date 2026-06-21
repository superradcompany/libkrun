use std::collections::btree_map;
use std::collections::BTreeMap;
use std::convert::TryInto;
use std::ffi::CStr;
use std::fs::{File, FileType};
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INVALID_NAME,
    ERROR_PATH_NOT_FOUND,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_READONLY;

use super::super::bindings;
use super::super::filesystem::{
    Context, DirEntry, Entry, ExportTable, FileSystem, FsOptions, OpenOptions, ZeroCopyWriter,
};
use super::super::fuse;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const INIT_CSTR: &[u8] = b"init.krun\0";
const INIT_BINARY: &[u8] = include_bytes!("../../../../init");

const DT_UNKNOWN: u32 = 0;
const DT_DIR: u32 = 4;
const DT_REG: u32 = 8;
const DT_LNK: u32 = 10;

const LINUX_EIO: i32 = 5;
const LINUX_EBADF: i32 = 9;
const LINUX_EACCES: i32 = 13;
const LINUX_EEXIST: i32 = 17;
const LINUX_ENOENT: i32 = 2;
const LINUX_ENOTDIR: i32 = 20;
const LINUX_EISDIR: i32 = 21;
const LINUX_EINVAL: i32 = 22;
const LINUX_ELOOP: i32 = 40;
const LINUX_EOPNOTSUPP: i32 = 95;

const LINUX_O_ACCMODE: i32 = 0o3;
const LINUX_O_WRONLY: i32 = 0o1;
const LINUX_O_RDWR: i32 = 0o2;

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

const WINDOWS_TICKS_PER_SECOND: u64 = 10_000_000;
const WINDOWS_TO_UNIX_EPOCH_SECONDS: u64 = 11_644_473_600;

type Inode = u64;
type Handle = u64;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct Config {
    pub root_dir: String,
    pub allow_root_dir_delete: bool,
    pub export_fsid: u64,
    pub export_table: Option<ExportTable>,
}

pub struct PassthroughFs {
    inodes: RwLock<InodeTable>,
    next_inode: AtomicU64,
    init_inode: u64,
    handles: RwLock<BTreeMap<Handle, Arc<HandleData>>>,
    next_handle: AtomicU64,
    init_handle: u64,
    root: PathBuf,
    _cfg: Config,
}

#[derive(Default)]
struct InodeTable {
    by_inode: BTreeMap<Inode, Arc<InodeData>>,
    by_path: BTreeMap<PathBuf, Arc<InodeData>>,
}

struct InodeData {
    inode: Inode,
    path: PathBuf,
    refcount: AtomicU64,
}

struct HandleData {
    inode: Inode,
    kind: HandleKind,
    dirstream: Mutex<DirStream>,
}

enum HandleKind {
    File(RwLock<File>),
    Directory(PathBuf),
}

struct CachedDirEntry {
    ino: bindings::ino64_t,
    offset: u64,
    type_: u32,
    name: Box<[u8]>,
}

#[derive(Default)]
struct DirStream {
    entries: Vec<CachedDirEntry>,
    ready: bool,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl PassthroughFs {
    pub fn new(cfg: Config) -> io::Result<Self> {
        let root = std::fs::canonicalize(&cfg.root_dir).map_err(host_error)?;
        let metadata = std::fs::symlink_metadata(&root).map_err(host_error)?;
        if !metadata.file_type().is_dir() {
            return Err(linux_error(LINUX_ENOTDIR));
        }

        Ok(Self {
            inodes: RwLock::new(InodeTable::default()),
            next_inode: AtomicU64::new(fuse::ROOT_ID + 2),
            init_inode: fuse::ROOT_ID + 1,
            handles: RwLock::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
            init_handle: 0,
            root,
            _cfg: cfg,
        })
    }

    fn do_lookup(&self, parent: Inode, name: &CStr) -> io::Result<Entry> {
        let child = self.child_path(parent, name)?;
        let metadata = std::fs::symlink_metadata(&child).map_err(host_error)?;
        let inode_data = self.intern_path(child, 1);
        let attr = stat_from_metadata(&metadata, inode_data.inode);

        Ok(Entry {
            inode: inode_data.inode,
            generation: 0,
            attr,
            attr_flags: 0,
            attr_timeout: self.cfg_attr_timeout(),
            entry_timeout: self.cfg_entry_timeout(),
        })
    }

    fn child_path(&self, parent: Inode, name: &CStr) -> io::Result<PathBuf> {
        let name = cstr_to_component(name)?;
        let parent = self.inode(parent)?.path.clone();
        Ok(parent.join(name))
    }

    fn inode(&self, inode: Inode) -> io::Result<Arc<InodeData>> {
        self.inodes
            .read()
            .unwrap()
            .by_inode
            .get(&inode)
            .cloned()
            .ok_or_else(|| linux_error(LINUX_EBADF))
    }

    fn intern_path(&self, path: PathBuf, lookup_count: u64) -> Arc<InodeData> {
        let mut table = self.inodes.write().unwrap();
        if let Some(data) = table.by_path.get(&path) {
            if lookup_count != 0 {
                data.refcount.fetch_add(lookup_count, Ordering::Acquire);
            }
            return data.clone();
        }

        let inode = self.next_inode.fetch_add(1, Ordering::Relaxed);
        let data = Arc::new(InodeData {
            inode,
            path: path.clone(),
            refcount: AtomicU64::new(lookup_count),
        });
        table.by_inode.insert(inode, data.clone());
        table.by_path.insert(path, data.clone());
        data
    }

    fn insert_root(&self) -> io::Result<bindings::stat64> {
        let metadata = std::fs::symlink_metadata(&self.root).map_err(host_error)?;
        let data = Arc::new(InodeData {
            inode: fuse::ROOT_ID,
            path: self.root.clone(),
            refcount: AtomicU64::new(2),
        });

        let mut table = self.inodes.write().unwrap();
        table.by_inode.clear();
        table.by_path.clear();
        table.by_inode.insert(fuse::ROOT_ID, data.clone());
        table.by_path.insert(self.root.clone(), data);

        Ok(stat_from_metadata(&metadata, fuse::ROOT_ID))
    }

    fn do_open(&self, inode: Inode, flags: u32) -> io::Result<(Option<Handle>, OpenOptions)> {
        let data = self.inode(inode)?;
        validate_read_only_open(flags)?;
        reject_symlink(&data.path)?;

        let metadata = std::fs::symlink_metadata(&data.path).map_err(host_error)?;
        if metadata.file_type().is_dir() {
            return Err(linux_error(LINUX_EISDIR));
        }

        let file = File::open(&data.path).map_err(host_error)?;
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let data = HandleData {
            inode,
            kind: HandleKind::File(RwLock::new(file)),
            dirstream: Mutex::new(DirStream::default()),
        };

        self.handles.write().unwrap().insert(handle, Arc::new(data));
        Ok((Some(handle), OpenOptions::empty()))
    }

    fn do_opendir(&self, inode: Inode, flags: u32) -> io::Result<(Option<Handle>, OpenOptions)> {
        validate_read_only_open(flags)?;

        let data = self.inode(inode)?;
        let metadata = std::fs::symlink_metadata(&data.path).map_err(host_error)?;
        if !metadata.file_type().is_dir() {
            return Err(linux_error(LINUX_ENOTDIR));
        }

        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let data = HandleData {
            inode,
            kind: HandleKind::Directory(data.path.clone()),
            dirstream: Mutex::new(DirStream::default()),
        };

        self.handles.write().unwrap().insert(handle, Arc::new(data));
        Ok((Some(handle), OpenOptions::empty()))
    }

    fn do_release(&self, inode: Inode, handle: Handle) -> io::Result<()> {
        let mut handles = self.handles.write().unwrap();
        if let btree_map::Entry::Occupied(entry) = handles.entry(handle) {
            if entry.get().inode == inode {
                entry.remove();
                return Ok(());
            }
        }

        Err(linux_error(LINUX_EBADF))
    }

    fn do_readdir<F>(
        &self,
        inode: Inode,
        handle: Handle,
        size: u32,
        mut offset: u64,
        mut add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        if size == 0 {
            return Ok(());
        }

        let handle_data = self
            .handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|data| data.inode == inode)
            .cloned()
            .ok_or_else(|| linux_error(LINUX_EBADF))?;

        let dir = match &handle_data.kind {
            HandleKind::Directory(path) => path.clone(),
            HandleKind::File(_) => return Err(linux_error(LINUX_ENOTDIR)),
        };

        let mut dirstream = handle_data.dirstream.lock().unwrap();
        if !dirstream.ready {
            self.fill_dir_stream(&dir, &mut dirstream)?;
        }

        while let Some(entry) = dirstream.get_entry(offset) {
            offset += 1;
            if add_entry(entry)? == 0 {
                break;
            }
        }

        Ok(())
    }

    fn fill_dir_stream(&self, dir: &Path, dirstream: &mut DirStream) -> io::Result<()> {
        for entry in std::fs::read_dir(dir).map_err(host_error)? {
            let entry = entry.map_err(host_error)?;
            let file_type = entry.file_type().map_err(host_error)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }

            let inode = self.intern_path(entry.path(), 0).inode;
            dirstream.entries.push(CachedDirEntry {
                ino: inode,
                offset: dirstream.entries.len() as u64 + 1,
                type_: dirent_type(file_type),
                name: name.into_bytes().into_boxed_slice(),
            });
        }

        dirstream.ready = true;
        Ok(())
    }

    fn do_getattr(&self, inode: Inode) -> io::Result<(bindings::stat64, Duration)> {
        if inode == self.init_inode {
            return Ok((init_stat(self.init_inode), self.cfg_attr_timeout()));
        }

        let data = self.inode(inode)?;
        let metadata = std::fs::symlink_metadata(&data.path).map_err(host_error)?;
        Ok((
            stat_from_metadata(&metadata, inode),
            self.cfg_attr_timeout(),
        ))
    }

    fn cfg_entry_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn cfg_attr_timeout(&self) -> Duration {
        Duration::from_secs(5)
    }
}

impl DirStream {
    fn get_entry(&self, offset: u64) -> Option<DirEntry<'_>> {
        self.entries.get(offset as usize).map(|entry| DirEntry {
            ino: entry.ino,
            offset: entry.offset,
            type_: entry.type_,
            name: &entry.name,
        })
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl FileSystem for PassthroughFs {
    type Inode = Inode;
    type Handle = Handle;

    fn init(&self, _capable: FsOptions) -> io::Result<FsOptions> {
        self.insert_root()?;
        Ok(FsOptions::empty())
    }

    fn destroy(&self) {
        self.handles.write().unwrap().clear();
        self.inodes.write().unwrap().by_inode.clear();
        self.inodes.write().unwrap().by_path.clear();
    }

    fn lookup(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        let init_name = unsafe { CStr::from_bytes_with_nul_unchecked(INIT_CSTR) };
        if parent == fuse::ROOT_ID && name == init_name {
            return Ok(Entry {
                inode: self.init_inode,
                generation: 0,
                attr: init_stat(self.init_inode),
                attr_flags: 0,
                attr_timeout: self.cfg_attr_timeout(),
                entry_timeout: self.cfg_entry_timeout(),
            });
        }

        self.do_lookup(parent, name)
    }

    fn forget(&self, _ctx: Context, inode: Inode, count: u64) {
        if inode == self.init_inode {
            return;
        }

        forget_one(&mut self.inodes.write().unwrap(), inode, count);
    }

    fn batch_forget(&self, _ctx: Context, requests: Vec<(Inode, u64)>) {
        let mut inodes = self.inodes.write().unwrap();
        for (inode, count) in requests {
            if inode != self.init_inode {
                forget_one(&mut inodes, inode, count);
            }
        }
    }

    fn getattr(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Option<Handle>,
    ) -> io::Result<(bindings::stat64, Duration)> {
        self.do_getattr(inode)
    }

    fn open(
        &self,
        _ctx: Context,
        inode: Inode,
        _kill_priv: bool,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        if inode == self.init_inode {
            return Ok((Some(self.init_handle), OpenOptions::empty()));
        }

        self.do_open(inode, flags)
    }

    fn read<W: io::Write + ZeroCopyWriter>(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        mut w: W,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        if inode == self.init_inode {
            let off: usize = offset.try_into().map_err(|_| linux_error(LINUX_EINVAL))?;
            if off >= INIT_BINARY.len() {
                return Ok(0);
            }

            let len = (size as usize).min(INIT_BINARY.len() - off);
            return w.write(&INIT_BINARY[off..off + len]);
        }

        let handle_data = self
            .handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|data| data.inode == inode)
            .cloned()
            .ok_or_else(|| linux_error(LINUX_EBADF))?;

        let file = match &handle_data.kind {
            HandleKind::File(file) => file.read().unwrap(),
            HandleKind::Directory(_) => return Err(linux_error(LINUX_EISDIR)),
        };

        w.write_from(&file, size as usize, offset)
    }

    fn release(
        &self,
        _ctx: Context,
        inode: Inode,
        _flags: u32,
        handle: Handle,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        if inode == self.init_inode && handle == self.init_handle {
            return Ok(());
        }

        self.do_release(inode, handle)
    }

    fn opendir(
        &self,
        _ctx: Context,
        inode: Inode,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        self.do_opendir(inode, flags)
    }

    fn readdir<F>(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        self.do_readdir(inode, handle, size, offset, add_entry)
    }

    fn releasedir(
        &self,
        _ctx: Context,
        inode: Inode,
        _flags: u32,
        handle: Handle,
    ) -> io::Result<()> {
        self.do_release(inode, handle)
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

fn forget_one(inodes: &mut InodeTable, inode: Inode, count: u64) {
    let Some(data) = inodes.by_inode.get(&inode) else {
        return;
    };

    let refcount = data.refcount.load(Ordering::Relaxed);
    let new_count = refcount.saturating_sub(count);
    data.refcount.store(new_count, Ordering::Release);
    if new_count == 0 && inode != fuse::ROOT_ID {
        let path = data.path.clone();
        inodes.by_inode.remove(&inode);
        inodes.by_path.remove(&path);
    }
}

fn validate_read_only_open(flags: u32) -> io::Result<()> {
    let flags = flags as i32;
    let accmode = flags & LINUX_O_ACCMODE;
    if accmode == LINUX_O_WRONLY || accmode == LINUX_O_RDWR {
        return Err(linux_error(LINUX_EOPNOTSUPP));
    }

    let unsupported = bindings::LINUX_O_CREAT
        | bindings::LINUX_O_TRUNC
        | bindings::LINUX_O_APPEND
        | bindings::LINUX_O_DIRECT;
    if (flags & unsupported) != 0 {
        return Err(linux_error(LINUX_EOPNOTSUPP));
    }

    Ok(())
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(host_error)?;
    if metadata.file_type().is_symlink() {
        return Err(linux_error(LINUX_ELOOP));
    }

    Ok(())
}

fn cstr_to_component(name: &CStr) -> io::Result<&str> {
    let component = name.to_str().map_err(|_| linux_error(LINUX_EINVAL))?;
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('\\')
        || component.contains('/')
    {
        return Err(linux_error(LINUX_EINVAL));
    }

    Ok(component)
}

fn stat_from_metadata(metadata: &std::fs::Metadata, inode: Inode) -> bindings::stat64 {
    let size = metadata.file_size() as i64;
    let (atime, atime_nsec) = filetime_to_unix(metadata.last_access_time());
    let (mtime, mtime_nsec) = filetime_to_unix(metadata.last_write_time());
    let (ctime, ctime_nsec) = filetime_to_unix(metadata.creation_time());

    bindings::stat64 {
        st_ino: inode,
        st_size: size,
        st_blocks: blocks_for_size(metadata.file_size()),
        st_atime: atime,
        st_mtime: mtime,
        st_ctime: ctime,
        st_atime_nsec: atime_nsec,
        st_mtime_nsec: mtime_nsec,
        st_ctime_nsec: ctime_nsec,
        st_mode: mode_from_metadata(metadata),
        st_nlink: 1,
        st_uid: 0,
        st_gid: 0,
        st_rdev: 0,
        st_blksize: 4096,
    }
}

fn init_stat(inode: Inode) -> bindings::stat64 {
    bindings::stat64 {
        st_ino: inode,
        st_size: INIT_BINARY.len() as i64,
        st_blocks: blocks_for_size(INIT_BINARY.len() as u64),
        st_mode: S_IFREG | 0o755,
        st_nlink: 1,
        st_blksize: 4096,
        ..Default::default()
    }
}

fn mode_from_metadata(metadata: &std::fs::Metadata) -> u32 {
    let file_type = metadata.file_type();
    let type_bits = if file_type.is_dir() {
        S_IFDIR
    } else if file_type.is_symlink() {
        S_IFLNK
    } else if file_type.is_file() {
        S_IFREG
    } else {
        0
    };

    let readonly = metadata.file_attributes() & FILE_ATTRIBUTE_READONLY != 0;
    let perms = if readonly { 0o555 } else { 0o777 };
    (type_bits & S_IFMT) | perms
}

fn dirent_type(file_type: FileType) -> u32 {
    if file_type.is_dir() {
        DT_DIR
    } else if file_type.is_symlink() {
        DT_LNK
    } else if file_type.is_file() {
        DT_REG
    } else {
        DT_UNKNOWN
    }
}

fn blocks_for_size(size: u64) -> i64 {
    size.div_ceil(512).try_into().unwrap_or(i64::MAX)
}

fn filetime_to_unix(filetime: u64) -> (i64, i64) {
    let seconds = filetime / WINDOWS_TICKS_PER_SECOND;
    if seconds < WINDOWS_TO_UNIX_EPOCH_SECONDS {
        return (0, 0);
    }

    let unix_seconds = seconds - WINDOWS_TO_UNIX_EPOCH_SECONDS;
    let nanos = (filetime % WINDOWS_TICKS_PER_SECOND) * 100;
    (
        unix_seconds.try_into().unwrap_or(i64::MAX),
        nanos.try_into().unwrap_or(i64::MAX),
    )
}

fn host_error(error: io::Error) -> io::Error {
    let errno = match error.raw_os_error() {
        Some(code)
            if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PATH_NOT_FOUND as i32 =>
        {
            LINUX_ENOENT
        }
        Some(code) if code == ERROR_ACCESS_DENIED as i32 => LINUX_EACCES,
        Some(code) if code == ERROR_ALREADY_EXISTS as i32 => LINUX_EEXIST,
        Some(code) if code == ERROR_INVALID_NAME as i32 => LINUX_EINVAL,
        _ => match error.kind() {
            io::ErrorKind::NotFound => LINUX_ENOENT,
            io::ErrorKind::PermissionDenied => LINUX_EACCES,
            io::ErrorKind::AlreadyExists => LINUX_EEXIST,
            io::ErrorKind::InvalidInput => LINUX_EINVAL,
            _ => LINUX_EIO,
        },
    };

    linux_error(errno)
}

fn linux_error(errno: i32) -> io::Error {
    io::Error::from_raw_os_error(errno)
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    struct CaptureWriter {
        bytes: Vec<u8>,
    }

    impl TempDir {
        fn new() -> Self {
            let mut path = std::env::temp_dir();
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            path.push(format!("msb-krun-fs-test-{}-{unique}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    impl Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl ZeroCopyWriter for CaptureWriter {
        fn write_from(&mut self, file: &File, count: usize, offset: u64) -> io::Result<usize> {
            let mut file = file.try_clone()?;
            file.seek(SeekFrom::Start(offset))?;
            let mut take = file.take(count as u64);
            take.read_to_end(&mut self.bytes)
        }
    }

    fn context() -> Context {
        Context {
            uid: 0,
            gid: 0,
            pid: 0,
        }
    }

    #[test]
    fn lookup_open_read_and_release_file() {
        let temp = TempDir::new();
        fs::write(temp.path.join("hello.txt"), b"hello from windows fs").unwrap();

        let fs = PassthroughFs::new(Config {
            root_dir: temp.path.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .unwrap();
        fs.init(FsOptions::empty()).unwrap();

        let name = CStr::from_bytes_with_nul(b"hello.txt\0").unwrap();
        let entry = fs.lookup(context(), fuse::ROOT_ID, name).unwrap();
        assert_eq!(entry.attr.st_size, 21);
        assert_eq!(entry.attr.st_mode & S_IFMT, S_IFREG);

        let (handle, _) = fs.open(context(), entry.inode, false, 0).unwrap();
        let handle = handle.unwrap();
        let mut writer = CaptureWriter { bytes: Vec::new() };
        let read = fs
            .read(context(), entry.inode, handle, &mut writer, 5, 6, None, 0)
            .unwrap();

        assert_eq!(read, 5);
        assert_eq!(writer.bytes, b"from ");
        fs.release(context(), entry.inode, 0, handle, false, false, None)
            .unwrap();
    }

    #[test]
    fn readdir_lists_children() {
        let temp = TempDir::new();
        fs::write(temp.path.join("alpha.txt"), b"alpha").unwrap();
        fs::create_dir(temp.path.join("nested")).unwrap();

        let fs = PassthroughFs::new(Config {
            root_dir: temp.path.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .unwrap();
        fs.init(FsOptions::empty()).unwrap();

        let (handle, _) = fs.opendir(context(), fuse::ROOT_ID, 0).unwrap();
        let handle = handle.unwrap();
        let mut names = Vec::new();
        fs.readdir(context(), fuse::ROOT_ID, handle, 4096, 0, |entry| {
            names.push(String::from_utf8(entry.name.to_vec()).unwrap());
            Ok(1)
        })
        .unwrap();

        names.sort();
        assert_eq!(names, vec!["alpha.txt", "nested"]);
        fs.releasedir(context(), fuse::ROOT_ID, 0, handle).unwrap();
    }
}
