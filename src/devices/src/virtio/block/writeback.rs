// Copyright 2026 The Microsandbox Authors. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use log::warn;

// Heavy sequential writers in the measured guest issued about 16 GiB between durability
// barriers. Starting one advisory writeback near the end of that interval leaves the final fsync
// responsible for durability while giving Linux time to drain dirty pages on an otherwise-idle
// device. The environment override is intentionally internal and exists for controlled host
// tuning; zero disables advisory writeback without changing the guest-visible cache contract.
const DEFAULT_TRIGGER_BYTES: u64 = 12 * 1024 * 1024 * 1024;
const MINIMUM_RANGE_BYTES: u64 = 64 * 1024 * 1024;
const TRIGGER_ENV: &str = "KRUN_BLOCK_WRITEBACK_PREFLUSH_BYTES";

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
}

impl WritebackWindow {
    fn new(trigger_bytes: u64) -> Self {
        Self {
            trigger_bytes,
            logical_bytes: 0,
            dirty_range: None,
        }
    }

    fn record_write(&mut self, offset: u64, length: u64) -> Option<DirtyRange> {
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

        self.logical_bytes = 0;
        self.dirty_range = None;
        Some(dirty_range)
    }

    fn pending_range(&self) -> Option<DirtyRange> {
        self.dirty_range
    }

    fn complete_flush(&mut self) {
        self.logical_bytes = 0;
        self.dirty_range = None;
    }
}

/// Schedules bounded advisory data writeback for one buffered raw-file block device.
///
/// `sync_file_range(..., SYNC_FILE_RANGE_WRITE)` does not claim durability. Guest flush
/// completion still depends on the existing full file sync after every earlier write has been
/// processed by the block worker.
pub(crate) struct BufferedWritebackController {
    file: Arc<File>,
    window: WritebackWindow,
    disabled: bool,
}

impl BufferedWritebackController {
    pub(crate) fn from_environment(file: Arc<File>) -> Option<Self> {
        let trigger_bytes = env::var(TRIGGER_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_TRIGGER_BYTES);
        if trigger_bytes == 0 {
            return None;
        }

        Some(Self {
            file,
            window: WritebackWindow::new(trigger_bytes),
            disabled: false,
        })
    }

    pub(crate) fn record_write(&mut self, offset: u64, length: u64) {
        if self.disabled {
            return;
        }

        if let Some(range) = self.window.record_write(offset, length) {
            self.submit(range);
        }
    }

    pub(crate) fn prepare_flush(&mut self) {
        if self.disabled {
            return;
        }

        if let Some(range) = self.window.pending_range() {
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

        // Safe: the controller owns a live duplicate of the exact file object used by Imago, and
        // the remaining arguments are validated integer values and a Linux-defined flag.
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
    fn threshold_submits_one_coalesced_range_and_starts_a_new_window() {
        let mut window = WritebackWindow::new(96 * 1024 * 1024);
        assert_eq!(window.record_write(0, 64 * 1024 * 1024), None);
        assert_eq!(
            window.record_write(64 * 1024 * 1024, 32 * 1024 * 1024),
            Some(DirtyRange {
                start: 0,
                end: 96 * 1024 * 1024
            })
        );
        assert_eq!(window.pending_range(), None);

        assert_eq!(window.record_write(256 * 1024 * 1024, 512), None);
        assert_eq!(
            window.pending_range(),
            Some(DirtyRange {
                start: 256 * 1024 * 1024,
                end: 256 * 1024 * 1024 + 512
            })
        );
    }

    #[test]
    fn flush_keeps_pending_range_until_durability_completes() {
        let mut window = WritebackWindow::new(u64::MAX);
        window.record_write(4096, 128 * 1024 * 1024);
        let expected = DirtyRange {
            start: 4096,
            end: 4096 + 128 * 1024 * 1024,
        };
        assert_eq!(window.pending_range(), Some(expected));
        assert_eq!(window.pending_range(), Some(expected));

        window.complete_flush();
        assert_eq!(window.pending_range(), None);
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
        assert_eq!(controller.window.pending_range(), None);

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
