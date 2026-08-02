// Copyright 2026 The Microsandbox Authors. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use log::warn;

pub(crate) const MINIMUM_RANGE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirtyRange {
    start: u64,
    end: u64,
}

impl DirtyRange {
    fn new(offset: u64, length: u64) -> Option<Self> {
        if length == 0 {
            return None;
        }

        Some(Self {
            start: offset,
            end: offset.saturating_add(length),
        })
    }

    fn merge(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

#[derive(Debug)]
struct WritebackWindow {
    trigger_bytes: u64,
    logical_bytes: u64,
    dirty_range: Option<DirtyRange>,
    submitted: bool,
}

impl WritebackWindow {
    fn new(trigger_bytes: u64) -> Self {
        Self {
            trigger_bytes,
            logical_bytes: 0,
            dirty_range: None,
            submitted: false,
        }
    }

    fn record_write(&mut self, offset: u64, length: u64) -> Option<DirtyRange> {
        // One advisory submission is enough to start background writeout for this durability
        // epoch. Later writes are still covered by the mandatory full sync at guest flush time.
        if self.submitted {
            return None;
        }

        let range = DirtyRange::new(offset, length)?;
        self.logical_bytes = self.logical_bytes.saturating_add(length);
        self.dirty_range = Some(
            self.dirty_range
                .map_or(range, |existing| existing.merge(range)),
        );

        let dirty_range = self.dirty_range?;
        if self.logical_bytes < self.trigger_bytes || dirty_range.len() < MINIMUM_RANGE_BYTES {
            return None;
        }

        self.submitted = true;
        Some(dirty_range)
    }

    #[cfg(test)]
    fn pending_range(&self) -> Option<DirtyRange> {
        self.dirty_range
    }

    fn complete_flush(&mut self) {
        self.logical_bytes = 0;
        self.dirty_range = None;
        self.submitted = false;
    }
}

/// Immutable inputs used to create a fresh controller when a block device is reactivated.
#[derive(Clone)]
pub(crate) struct BufferedWritebackConfig {
    file: Arc<File>,
    trigger_bytes: u64,
}

impl BufferedWritebackConfig {
    pub(crate) fn new(file: Arc<File>, trigger_bytes: u64) -> Self {
        Self {
            file,
            trigger_bytes,
        }
    }

    pub(crate) fn controller(&self) -> BufferedWritebackController {
        BufferedWritebackController {
            file: Arc::clone(&self.file),
            window: WritebackWindow::new(self.trigger_bytes),
            disabled: false,
        }
    }
}

/// Schedules one advisory data-writeback range per guest durability epoch.
///
/// `sync_file_range(..., SYNC_FILE_RANGE_WRITE)` does not claim durability. Guest flush
/// completion still depends on the existing full file sync after every earlier write has been
/// processed by the block worker. This controller is a latency hint rather than a dirty-memory
/// containment boundary: after the first submission, later writes wait for the mandatory guest
/// flush. Hosts running untrusted guests must enforce memory and dirty-page limits independently.
pub(crate) struct BufferedWritebackController {
    file: Arc<File>,
    window: WritebackWindow,
    disabled: bool,
}

impl BufferedWritebackController {
    pub(crate) fn record_write(&mut self, offset: u64, length: u64) {
        if self.disabled {
            return;
        }

        if let Some(range) = self.window.record_write(offset, length) {
            self.submit(range);
        }
    }

    pub(crate) fn complete_flush(&mut self) {
        self.window.complete_flush();
    }

    fn submit(&mut self, range: DirtyRange) {
        let Ok(offset) = libc::off64_t::try_from(range.start) else {
            self.disable("offset exceeds sync_file_range limits");
            return;
        };
        let Ok(length) = libc::off64_t::try_from(range.len()) else {
            self.disable("length exceeds sync_file_range limits");
            return;
        };

        // Safe: the controller owns a live descriptor for the exact backing inode reopened by
        // Imago, and the remaining arguments are validated integers plus a Linux-defined flag.
        let result = unsafe {
            libc::sync_file_range(
                self.file.as_raw_fd(),
                offset,
                length,
                libc::SYNC_FILE_RANGE_WRITE,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            warn!("Disabling buffered block preflush after sync_file_range failed: {error}");
            self.disabled = true;
        }
    }

    fn disable(&mut self, reason: &str) {
        warn!("Disabling buffered block preflush: {reason}");
        self.disabled = true;
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn dirty_range_merges_out_of_order_writes() {
        let first = DirtyRange::new(128, 64).unwrap();
        let second = DirtyRange::new(32, 48).unwrap();
        assert_eq!(
            first.merge(second),
            DirtyRange {
                start: 32,
                end: 192
            }
        );
    }

    #[test]
    fn threshold_submits_only_once_until_durability_completes() {
        let mut window = WritebackWindow::new(96 * 1024 * 1024);
        assert_eq!(window.record_write(0, 64 * 1024 * 1024), None);
        assert_eq!(
            window.record_write(64 * 1024 * 1024, 32 * 1024 * 1024),
            Some(DirtyRange {
                start: 0,
                end: 96 * 1024 * 1024
            })
        );
        assert_eq!(
            window.pending_range(),
            Some(DirtyRange {
                start: 0,
                end: 96 * 1024 * 1024
            })
        );

        assert_eq!(window.record_write(256 * 1024 * 1024, 512), None);
        assert_eq!(
            window.pending_range(),
            Some(DirtyRange {
                start: 0,
                end: 96 * 1024 * 1024
            })
        );

        window.complete_flush();
        assert_eq!(window.pending_range(), None);
        assert_eq!(
            window.record_write(512 * 1024 * 1024, 96 * 1024 * 1024),
            Some(DirtyRange {
                start: 512 * 1024 * 1024,
                end: 608 * 1024 * 1024
            })
        );
    }

    #[test]
    fn empty_write_does_not_change_the_window() {
        let mut window = WritebackWindow::new(1);
        assert_eq!(window.record_write(1024, 0), None);
        assert_eq!(window.pending_range(), None);
    }

    #[test]
    fn regular_file_accepts_advisory_writeback() {
        let path = temporary_file_path("regular");
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(MINIMUM_RANGE_BYTES).unwrap();

        let mut controller = BufferedWritebackController {
            file: Arc::new(file),
            window: WritebackWindow::new(1),
            disabled: false,
        };
        controller.record_write(0, MINIMUM_RANGE_BYTES);
        assert!(!controller.disabled);
        assert_eq!(
            controller.window.pending_range(),
            Some(DirtyRange {
                start: 0,
                end: MINIMUM_RANGE_BYTES
            })
        );

        drop(controller);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unsupported_file_disables_only_the_advisory_controller() {
        let unsupported = OpenOptions::new().write(true).open("/dev/null").unwrap();
        let mut controller = BufferedWritebackController {
            file: Arc::new(unsupported),
            window: WritebackWindow::new(1),
            disabled: false,
        };

        controller.record_write(0, MINIMUM_RANGE_BYTES);
        assert!(controller.disabled);
    }

    fn temporary_file_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "libkrun-writeback-{label}-{}-{nonce}",
            std::process::id()
        ))
    }
}
