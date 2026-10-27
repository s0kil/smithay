//! Optional, non-waiting snapshots of Linux native-fence timing.
//!
//! These observations describe fence readiness, not physical PCIe transfer
//! time. Exporting a DMA-BUF READ fence snapshots its current writers; it does
//! not synchronize later submissions by another client.

use std::{
    collections::HashSet,
    io,
    os::fd::{BorrowedFd, FromRawFd, OwnedFd},
};

use crate::backend::allocator::dmabuf::Dmabuf;

const MAX_FENCES: usize = 256;
const MAX_PLANES: usize = 4;

#[derive(Debug, thiserror::Error)]
pub(super) enum FenceTimingError {
    #[error("native-fence timing ioctl is unsupported")]
    Unsupported,
    #[error("invalid native-fence timing data: {0}")]
    InvalidData(&'static str),
    #[error("native-fence timing I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub(super) struct FenceDetail {
    pub driver_name: String,
    pub obj_name: String,
    /// Zero is active, one is signaled, and a negative value is a fence error.
    pub status: i32,
    /// Kernel CLOCK_MONOTONIC signal timestamp; zero means unavailable.
    pub timestamp_ns: u64,
}

#[derive(Debug, Clone)]
pub(super) struct FenceSnapshot {
    /// Aggregate kernel status: zero active, one signaled, negative error.
    pub status: i32,
    pub fences: Vec<FenceDetail>,
    /// Latest signal time only when every fence completed successfully and
    /// every timestamp is known. An empty fence set does not establish a time.
    pub completed_timestamp_ns: Option<u64>,
}

// Linux UAPI layouts from linux/dma-buf.h and linux/sync_file.h. The pointer
// field is __u64 on both 32-bit and 64-bit userspace, not a native Rust pointer.
#[repr(C)]
#[derive(Debug, Default)]
struct RawExportSyncFile {
    flags: u32,
    fd: i32,
}

#[repr(C)]
#[derive(Debug, Default)]
struct RawSyncFileInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    num_fences: u32,
    pad: u32,
    sync_fence_info: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone)]
struct RawFenceInfo {
    obj_name: [u8; 32],
    driver_name: [u8; 32],
    status: i32,
    flags: u32,
    timestamp_ns: u64,
}

fn ioctl_error(error: rustix::io::Errno) -> FenceTimingError {
    match error {
        rustix::io::Errno::NOTTY | rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::NOSYS => {
            FenceTimingError::Unsupported
        }
        other => FenceTimingError::Io(other.into()),
    }
}

#[cfg(target_os = "linux")]
fn export_read_fence(fd: BorrowedFd<'_>) -> Result<OwnedFd, FenceTimingError> {
    let mut raw = RawExportSyncFile { flags: 1, fd: -1 }; // DMA_BUF_SYNC_READ
    // SAFETY: this is the Linux DMA_BUF_IOCTL_EXPORT_SYNC_FILE UAPI with its
    // matching initialized C layout. The kernel returns a newly owned fd.
    unsafe {
        rustix::ioctl::ioctl(
            fd,
            rustix::ioctl::Updater::<
                { rustix::ioctl::opcode::read_write::<RawExportSyncFile>(b'b', 2) },
                RawExportSyncFile,
            >::new(&mut raw),
        )
    }
    .map_err(ioctl_error)?;
    if raw.fd < 0 {
        return Err(FenceTimingError::InvalidData("export returned no descriptor"));
    }
    // SAFETY: successful export returns a new fd which has no other Rust owner.
    let owned = unsafe { OwnedFd::from_raw_fd(raw.fd) };
    let flags = rustix::io::fcntl_getfd(&owned).map_err(ioctl_error)?;
    if !flags.contains(rustix::io::FdFlags::CLOEXEC) {
        rustix::io::fcntl_setfd(&owned, flags | rustix::io::FdFlags::CLOEXEC).map_err(ioctl_error)?;
    }
    Ok(owned)
}

/// Snapshot writers of each unique DMA-BUF plane before consuming the image.
///
/// This does not wait, import a fence, flush GL, or change the reservation set.
/// If any plane cannot be observed, all partial exports are dropped and the
/// caller receives an error rather than an incomplete readiness estimate.
pub(super) fn export_dmabuf_read_fences(buffer: &Dmabuf) -> Result<Vec<OwnedFd>, FenceTimingError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = buffer;
        Err(FenceTimingError::Unsupported)
    }
    #[cfg(target_os = "linux")]
    {
        if buffer.num_planes() > MAX_PLANES {
            return Err(FenceTimingError::InvalidData("too many DMA-BUF planes"));
        }
        let mut identities = HashSet::with_capacity(MAX_PLANES);
        let mut result = Vec::with_capacity(buffer.num_planes());
        for fd in buffer.handles() {
            let stat = rustix::fs::fstat(fd).map_err(ioctl_error)?;
            if identities.insert((stat.st_dev, stat.st_ino)) {
                result.push(export_read_fence(fd)?);
            }
        }
        if result.is_empty() {
            return Err(FenceTimingError::InvalidData("DMA-BUF has no planes"));
        }
        Ok(result)
    }
}

#[cfg(target_os = "linux")]
fn query_info(fd: BorrowedFd<'_>, info: &mut RawSyncFileInfo) -> Result<(), FenceTimingError> {
    // SAFETY: SYNC_IOC_FILE_INFO expects this initialized C layout. On the
    // second call the embedded pointer references an initialized, writable
    // RawFenceInfo array of num_fences elements which remains alive throughout.
    unsafe {
        rustix::ioctl::ioctl(
            fd,
            rustix::ioctl::Updater::<
                { rustix::ioctl::opcode::read_write::<RawSyncFileInfo>(b'>', 4) },
                RawSyncFileInfo,
            >::new(info),
        )
    }
    .map_err(ioctl_error)
}

fn checked_count(count: u32) -> Result<usize, FenceTimingError> {
    let count = count as usize;
    if count > MAX_FENCES {
        return Err(FenceTimingError::InvalidData("native fence count exceeds limit"));
    }
    Ok(count)
}

fn name(bytes: &[u8; 32]) -> String {
    let len = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

fn snapshot(info: &RawSyncFileInfo, raw: &[RawFenceInfo]) -> Result<FenceSnapshot, FenceTimingError> {
    if checked_count(info.num_fences)? != raw.len() {
        return Err(FenceTimingError::InvalidData("native fence count changed"));
    }
    if info.status > 1 || raw.iter().any(|fence| fence.status > 1) {
        return Err(FenceTimingError::InvalidData("unknown native fence status"));
    }
    if info.flags != 0 || info.pad != 0 || raw.iter().any(|fence| fence.flags != 0) {
        return Err(FenceTimingError::InvalidData("unknown native fence flags"));
    }
    if raw.iter().any(|fence| fence.timestamp_ns > i64::MAX as u64) {
        return Err(FenceTimingError::InvalidData("invalid monotonic timestamp"));
    }
    let completed_timestamp_ns = (info.status == 1
        && !raw.is_empty()
        && raw
            .iter()
            .all(|fence| fence.status == 1 && fence.timestamp_ns != 0))
    .then(|| raw.iter().map(|fence| fence.timestamp_ns).max())
    .flatten();
    Ok(FenceSnapshot {
        status: info.status,
        fences: raw
            .iter()
            .map(|fence| FenceDetail {
                driver_name: name(&fence.driver_name),
                obj_name: name(&fence.obj_name),
                status: fence.status,
                timestamp_ns: fence.timestamp_ns,
            })
            .collect(),
        completed_timestamp_ns,
    })
}

/// Read a native sync_file without waiting for it to signal.
///
/// A bounded two-pass ioctl obtains the count, then its per-fence details.
/// Active, error, empty, and zero-timestamp fences remain explicit observations
/// and never acquire an invented completion time.
pub(super) fn inspect_sync_file(fd: BorrowedFd<'_>) -> Result<FenceSnapshot, FenceTimingError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = fd;
        Err(FenceTimingError::Unsupported)
    }
    #[cfg(target_os = "linux")]
    {
        let mut info = RawSyncFileInfo::default();
        query_info(fd, &mut info)?;
        let count = checked_count(info.num_fences)?;
        if count == 0 {
            return snapshot(&info, &[]);
        }
        let mut fences = vec![RawFenceInfo::default(); count];
        info = RawSyncFileInfo {
            num_fences: count as u32,
            sync_fence_info: fences.as_mut_ptr() as usize as u64,
            ..RawSyncFileInfo::default()
        };
        query_info(fd, &mut info)?;
        snapshot(&info, &fences)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn raw(status: i32, timestamp_ns: u64) -> RawFenceInfo {
        RawFenceInfo {
            status,
            timestamp_ns,
            ..RawFenceInfo::default()
        }
    }

    fn observe(status: i32, fences: &[RawFenceInfo]) -> Result<FenceSnapshot, FenceTimingError> {
        snapshot(
            &RawSyncFileInfo {
                status,
                num_fences: fences.len() as u32,
                ..RawSyncFileInfo::default()
            },
            fences,
        )
    }

    #[test]
    fn uapi_struct_sizes_are_exact() {
        assert_eq!(std::mem::size_of::<RawExportSyncFile>(), 8);
        assert_eq!(std::mem::size_of::<RawSyncFileInfo>(), 56);
        assert_eq!(std::mem::size_of::<RawFenceInfo>(), 80);
    }

    #[test]
    fn latest_successful_fence_defines_readiness() {
        let result = observe(1, &[raw(1, 20), raw(1, 40), raw(1, 30)]).unwrap();
        assert_eq!(result.completed_timestamp_ns, Some(40));
    }

    #[test]
    fn incomplete_error_and_missing_times_are_not_ready_timestamps() {
        for (status, fences) in [
            (0, vec![raw(1, 20), raw(0, 0)]),
            (-5, vec![raw(1, 20), raw(-5, 30)]),
            (1, vec![raw(1, 0)]),
            (1, vec![]),
            (0, vec![raw(1, 20)]),
        ] {
            assert_eq!(observe(status, &fences).unwrap().completed_timestamp_ns, None);
        }
    }

    #[test]
    fn malformed_counts_and_statuses_are_rejected() {
        assert!(checked_count((MAX_FENCES + 1) as u32).is_err());
        assert!(observe(2, &[raw(1, 20)]).is_err());
        assert!(observe(1, &[raw(2, 20)]).is_err());
        assert!(observe(1, &[raw(1, u64::MAX)]).is_err());
        let info = RawSyncFileInfo {
            num_fences: 2,
            ..RawSyncFileInfo::default()
        };
        assert!(snapshot(&info, &[raw(1, 10)]).is_err());
        let flagged = RawSyncFileInfo {
            status: 1,
            flags: 1,
            ..RawSyncFileInfo::default()
        };
        assert!(snapshot(&flagged, &[]).is_err());
    }

    #[test]
    fn names_do_not_need_a_terminator_or_valid_utf8() {
        assert_eq!(name(&[b'a'; 32]), "a".repeat(32));
        assert_eq!(name(&[0; 32]), "");
        let mut bytes = [0; 32];
        bytes[0] = 0xff;
        assert_eq!(name(&bytes), "\u{fffd}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_ordinary_descriptor_reports_unsupported_without_waiting() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(matches!(
            inspect_sync_file(file.as_fd()),
            Err(FenceTimingError::Unsupported)
        ));
        assert!(matches!(
            export_read_fence(file.as_fd()),
            Err(FenceTimingError::Unsupported)
        ));
    }
}
