//! Test devices that several handles reach, as duplicated descriptors reach one file: a volume
//! reads through its own handle while the device's issuer writes through a duplicate of its
//! own (`BlockFile::try_clone`). hyper-block's simulated file has one owner, so a simulated
//! device is a thread that owns the file and does each operation a handle sends it, in the
//! order they arrive; a handle is a sender to it. A wrapper that watches or holds a device's
//! I/O is shared by its handles behind an `Arc` (`Handle`). Shared by the crate's unit tests
//! (lib.rs) and its integration tests.

use std::sync::Arc;
use std::sync::mpsc::{SyncSender, sync_channel};

use hyper_block::DiskError;
use hyper_block::block::BlockFile;
use hyper_block::buf::{AlignedBuf, Alignment};
use hyper_block::sim::{Crash, Fault, SimFile, SimStats};

/// A handle to a device shared by every handle made from it.
pub struct Handle<T>(pub Arc<T>);

impl<T> Handle<T> {
    pub fn new(device: T) -> Self {
        Self(Arc::new(device))
    }
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> std::ops::Deref for Handle<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: BlockFile + Sync> BlockFile for Handle<T> {
    fn alignment(&self) -> Alignment {
        self.0.alignment()
    }

    fn len(&self) -> Result<u64, DiskError> {
        self.0.len()
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<(), DiskError> {
        self.0.read_exact_at(buf, offset)
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> Result<(), DiskError> {
        self.0.write_all_at(buf, offset)
    }

    fn sync_data(&self) -> Result<(), DiskError> {
        self.0.sync_data()
    }

    fn try_clone(&self) -> Result<Self, DiskError> {
        Ok(self.clone())
    }
}

/// An operation on the simulated file, done on the device's thread.
type Job = Box<dyn FnOnce(&SimFile) + Send>;

/// A handle to a simulated file with power-loss semantics, which a thread of its own owns:
/// every handle's operation is a message to that thread, answered on a port of the caller's,
/// so the file sees one operation at a time, as a device's queue serves its submitters. The
/// thread ends once its last handle is dropped.
pub struct SimDevice {
    jobs: SyncSender<Job>,
    align: Alignment,
}

impl Clone for SimDevice {
    fn clone(&self) -> Self {
        Self {
            jobs: self.jobs.clone(),
            align: self.align,
        }
    }
}

impl SimDevice {
    pub fn new(file: SimFile) -> Self {
        let align = file.alignment();
        // Each sender waits for its answer, so at most one job a handle's thread is queued;
        // one slot is the device's queue.
        let (jobs, queued) = sync_channel::<Job>(1);
        std::thread::Builder::new()
            .name("sim device".into())
            .spawn(move || {
                for job in queued {
                    job(&file);
                }
            })
            .unwrap();
        Self { jobs, align }
    }

    /// Runs `op` on the device's thread and waits for its answer.
    fn with<R: Send + 'static>(&self, op: impl FnOnce(&SimFile) -> R + Send + 'static) -> R {
        let (reply, answer) = sync_channel(1);
        self.jobs
            .send(Box::new(move |file| {
                let _ = reply.send(op(file));
            }))
            .expect("the device's thread runs while a handle is held");
        answer
            .recv()
            .expect("the device's thread answers every job")
    }

    /// Refuses a caller's buffer at an address the device's alignment does not meet.
    fn aligned(&self, buf: &[u8], offset: u64) -> Result<(), DiskError> {
        if self.align.is_aligned(buf.as_ptr().addr()) {
            return Ok(());
        }
        Err(DiskError::Misaligned {
            offset,
            len: buf.len(),
            align: self.align.get(),
        })
    }

    pub fn inject(&self, fault: Fault) -> Result<(), DiskError> {
        self.with(move |file| file.inject(fault))
    }

    pub fn clear_faults(&self) -> Result<(), DiskError> {
        self.with(SimFile::clear_faults)
    }

    pub fn crash(&self, crash: Crash) -> Result<(), DiskError> {
        self.with(move |file| file.crash(crash))
    }

    pub fn stats(&self) -> Result<SimStats, DiskError> {
        self.with(SimFile::stats)
    }

    pub fn durable_image(&self) -> Result<Vec<u8>, DiskError> {
        self.with(SimFile::durable_image)
    }
}

impl BlockFile for SimDevice {
    fn alignment(&self) -> Alignment {
        self.align
    }

    fn len(&self) -> Result<u64, DiskError> {
        self.with(BlockFile::len)
    }

    // The bytes travel to the device's thread and back in buffers of the device's alignment;
    // the caller's buffer is held to it here, as the simulated file holds every transfer, as
    // direct I/O does.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<(), DiskError> {
        self.aligned(buf, offset)?;
        let (len, align) = (buf.len(), self.align);
        let read = self.with(move |file| {
            let mut read = AlignedBuf::zeroed(len, align).unwrap();
            read.set_len(len).unwrap();
            file.read_exact_at(read.as_mut_slice(), offset)
                .map(|()| read)
        })?;
        buf.copy_from_slice(read.as_slice());
        Ok(())
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> Result<(), DiskError> {
        self.aligned(buf, offset)?;
        let mut bytes = AlignedBuf::zeroed(buf.len(), self.align).unwrap();
        bytes.extend_from_slice(buf).unwrap();
        self.with(move |file| file.write_all_at(bytes.as_slice(), offset))
    }

    fn sync_data(&self) -> Result<(), DiskError> {
        self.with(BlockFile::sync_data)
    }

    fn try_clone(&self) -> Result<Self, DiskError> {
        Ok(self.clone())
    }
}

/// A simulated device of 4 KiB blocks and 512-byte sectors whose crashes and faults replay
/// from `seed`.
pub fn sim(seed: u64) -> SimDevice {
    SimDevice::new(
        SimFile::new(
            Alignment::new(4096).unwrap(),
            Alignment::new(512).unwrap(),
            seed,
        )
        .unwrap(),
    )
}
