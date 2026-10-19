//! Confining a guest's DAX window to a descriptor that covers only that window (§4.6).
//!
//! The arena's memory object holds every volume's blocks. A `SHMEM_MAP` names a window with `fd_offset` and
//! `len`, but those scope the *request*, not the *descriptor*: a front end handed the arena's own object can map
//! all of it, which measured 64× the authorized window with another tenant's blocks inside it
//! (`docs/bugs/2026-10-18-the-message-based-dax-map-hands-the-vmm-the-whole-arena.md`).
//!
//! `UDMABUF_CREATE` answers exactly that. Given a memfd, an offset and a size it returns a **new descriptor
//! covering only that range**, pinning the memfd's existing pages rather than copying them — so the confinement
//! costs no RAM and keeps writable zero-copy, which the per-mapping copy considered alongside it did not. The
//! arena's layout is untouched: this narrows what leaves the process, nothing else.
//!
//! Measured on Fedora 6.19 before it was built, as an unprivileged user with no `CAP_SYS_ADMIN`: a window at
//! offset 1 MiB of a 4 MiB object maps its own bytes and refuses both the whole object and any offset past
//! itself; it survives `SCM_RIGHTS` to another process; it takes `MAP_FIXED` into a region a VMM reserved, and
//! the hole can be restored afterwards; and a write through it lands in the arena's own pages.
//!
//! Two host facts the caller must handle rather than assume. The memfd needs `F_SEAL_SHRINK`, which
//! `slates-mem` now sets on every object it creates. And `/dev/udmabuf` is a device whose permissions are the
//! host's business (`root:kvm 0660` on Fedora) — it is not a privilege slates requires (R10), since it is the
//! same device the VMM already uses and DAX only matters where a VMM runs, but a daemon that cannot open it
//! must **refuse** the mapping rather than fall back to the wide descriptor.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// Format: `struct udmabuf_create` as the kernel declares it (`include/uapi/linux/udmabuf.h`): the memfd to
/// carve, its flags, and the byte range within it.
#[repr(C)]
struct UdmabufCreate {
  memfd: u32,
  flags: u32,
  offset: u64,
  size: u64,
}

/// Format: the device the ioctl is issued on (`drivers/dma-buf/udmabuf.c` registers this misc device).
const DEVICE: &str = "/dev/udmabuf";

/// Format: Linux's `_IOW(type, number, size)` encoding (`include/uapi/asm-generic/ioctl.h`) — the direction in
/// the top two bits (`_IOC_WRITE` = 1), the argument's size in the next fourteen, then the type letter and the
/// number. Written as the kernel's formula rather than the constant it evaluates to, so it cannot drift from
/// the struct above if a field is ever added.
const fn iow(group: u8, number: u8, size: usize) -> libc::c_ulong {
  // Format: the field widths of Linux's ioctl encoding (`include/uapi/asm-generic/ioctl.h`).
  const DIRECTION_WRITE: libc::c_ulong = 1;
  const NUMBER_BITS: u32 = 8;
  const TYPE_BITS: u32 = 8;
  const SIZE_BITS: u32 = 14;
  let size = (size as libc::c_ulong) & ((1 << SIZE_BITS) - 1);
  (DIRECTION_WRITE << (NUMBER_BITS + TYPE_BITS + SIZE_BITS))
    | (size << (NUMBER_BITS + TYPE_BITS))
    | ((group as libc::c_ulong) << NUMBER_BITS)
    | (number as libc::c_ulong)
}

/// Format: `UDMABUF_CREATE` is `_IOW('u', 0x42, struct udmabuf_create)`.
const UDMABUF_CREATE: libc::c_ulong = iow(b'u', 0x42, core::mem::size_of::<UdmabufCreate>());

/// Why a window could not be confined. Each is a refusal the caller turns into the guest's errno — never a
/// reason to send the unconfined descriptor instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfineError {
  /// `/dev/udmabuf` could not be opened: absent kernel support, or permissions this daemon does not have.
  DeviceUnavailable {
    /// The system's errno, for the record.
    code: Option<i32>,
  },
  /// The kernel refused the range: the object is not sealed against shrinking, the range is not page-aligned,
  /// or it is larger than `udmabuf`'s per-buffer limit (`/sys/module/udmabuf/parameters/size_limit_mb`).
  Refused {
    /// The system's errno, for the record.
    code: Option<i32>,
  },
}

/// A descriptor covering **only** `[offset, offset + len)` of `object`, for handing to a front end.
///
/// The returned fd is the caller's to drop once it has been sent: the receiver holds its own. The window's
/// bytes are the object's own pages, so a write through the guest's mapping reaches the arena — nothing is
/// copied.
pub(crate) fn confine(
  object: BorrowedFd<'_>,
  offset: u64,
  len: u64,
) -> Result<OwnedFd, ConfineError> {
  // Read-only, which is all `UDMABUF_CREATE` needs — measured both ways on a 6.19 host, and the device is
  // never written. That also keeps this inside the lint wall rather than needing an exception to it (R1).
  let device = rustix::fs::open(
    DEVICE,
    rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
    rustix::fs::Mode::empty(),
  )
  .map_err(|e| ConfineError::DeviceUnavailable {
    code: Some(e.raw_os_error()),
  })?;
  let memfd =
    u32::try_from(object.as_raw_fd()).map_err(|_| ConfineError::Refused { code: None })?;
  let request = UdmabufCreate {
    memfd,
    flags: 0,
    offset,
    size: len,
  };
  // SAFETY: `UDMABUF_CREATE` reads one `udmabuf_create` through the pointer and writes nothing back, and the
  // pointer names a live local of exactly that type and layout (`#[repr(C)]`, the kernel's field order). The
  // device descriptor is open and owned here for the call. The kernel returns a new descriptor on success and a
  // negative value on failure, which is checked before it is adopted.
  let created = unsafe { libc::ioctl(device.as_raw_fd(), UDMABUF_CREATE, &raw const request) };
  if created < 0 {
    return Err(ConfineError::Refused {
      code: std::io::Error::last_os_error().raw_os_error(),
    });
  }
  // SAFETY: `created` is a descriptor the kernel just returned to this process and nothing else owns it; it is
  // adopted once, here, and closed by the `OwnedFd`.
  Ok(unsafe { OwnedFd::from_raw_fd(created) })
}
