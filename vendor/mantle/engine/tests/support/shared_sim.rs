//! Test handles share one fault-injected device image across issuer worker duplicates.

use hyper_block::DiskError;
use hyper_block::block::BlockFile;
use hyper_block::buf::Alignment;
use hyper_block::sim::{Crash, Fault, SimFile, SimStats};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone)]
pub struct SharedSim {
    file: Arc<Mutex<SimFile>>,
    alignment: Alignment,
}

// Each integration-test binary uses the subset of device controls its oracle needs.
#[allow(dead_code)]
impl SharedSim {
    pub fn new(alignment: Alignment, sector: Alignment, seed: u64) -> Result<Self, DiskError> {
        Ok(Self {
            file: Arc::new(Mutex::new(SimFile::new(alignment, sector, seed)?)),
            alignment,
        })
    }

    fn file(&self) -> Result<MutexGuard<'_, SimFile>, DiskError> {
        self.file.lock().map_err(|_| DiskError::Io {
            op: "borrow the shared test device",
            path: std::path::PathBuf::new(),
            source: std::io::Error::other("poisoned test device"),
        })
    }

    pub fn inject(&self, fault: Fault) -> Result<(), DiskError> {
        self.file()?.inject(fault)
    }

    pub fn clear_faults(&self) -> Result<(), DiskError> {
        self.file()?.clear_faults()
    }

    pub fn crash(&self, crash: Crash) -> Result<(), DiskError> {
        self.file()?.crash(crash)
    }

    pub fn stats(&self) -> Result<SimStats, DiskError> {
        self.file()?.stats()
    }
}

impl BlockFile for SharedSim {
    fn alignment(&self) -> Alignment {
        self.alignment
    }

    fn len(&self) -> Result<u64, DiskError> {
        self.file()?.len()
    }

    fn read_exact_at(&self, out: &mut [u8], at: u64) -> Result<(), DiskError> {
        self.file()?.read_exact_at(out, at)
    }

    fn write_all_at(&self, bytes: &[u8], at: u64) -> Result<(), DiskError> {
        self.file()?.write_all_at(bytes, at)
    }

    fn sync_data(&self) -> Result<(), DiskError> {
        self.file()?.sync_data()
    }

    fn try_clone(&self) -> Result<Self, DiskError> {
        Ok(self.clone())
    }
}
