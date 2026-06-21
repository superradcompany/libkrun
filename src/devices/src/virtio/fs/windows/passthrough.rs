use std::io;

use super::super::filesystem::{ExportTable, FileSystem};

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
    _cfg: Config,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl PassthroughFs {
    pub fn new(cfg: Config) -> io::Result<Self> {
        Ok(Self { _cfg: cfg })
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl FileSystem for PassthroughFs {
    type Inode = u64;
    type Handle = u64;
}
