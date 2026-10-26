use std::{os::fd::OwnedFd, sync::Arc};

use crate::backend::renderer::sync::{Interrupted, SyncPoint};

/// Prepare a dependency once and retain its fd across TEST_ONLY and fallback ioctls.
pub(super) fn prepare(
    sync: &SyncPoint,
    fence: &mut Option<Arc<OwnedFd>>,
    supports_fencing: bool,
    wait_for_sync: bool,
) -> Result<(), Interrupted> {
    if supports_fencing && fence.is_none() {
        *fence = sync.export().map(Arc::new);
    }
    if wait_for_sync && (!supports_fencing || fence.is_none()) && !sync.is_reached() {
        sync.wait()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::backend::renderer::sync::Fence;

    use super::*;

    #[derive(Debug, Default)]
    struct State {
        reached: AtomicBool,
        exports: AtomicUsize,
        waits: AtomicUsize,
    }

    #[derive(Debug)]
    struct TestFence {
        state: Arc<State>,
        export_succeeds: bool,
        interrupt: bool,
    }

    impl Fence for TestFence {
        fn is_signaled(&self) -> bool {
            self.state.reached.load(Ordering::SeqCst)
        }

        fn wait(&self) -> Result<(), Interrupted> {
            self.state.waits.fetch_add(1, Ordering::SeqCst);
            if self.interrupt {
                Err(Interrupted)
            } else {
                self.state.reached.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        fn is_exportable(&self) -> bool {
            true
        }

        fn export(&self) -> Option<OwnedFd> {
            self.state.exports.fetch_add(1, Ordering::SeqCst);
            self.export_succeeds
                .then(|| std::fs::File::open("/dev/null").unwrap().into())
        }
    }

    fn point(export_succeeds: bool, interrupt: bool) -> (SyncPoint, Arc<State>) {
        let state = Arc::new(State::default());
        (
            TestFence {
                state: state.clone(),
                export_succeeds,
                interrupt,
            }
            .into(),
            state,
        )
    }

    #[test]
    fn failed_native_export_waits_before_submission() {
        let (sync, state) = point(false, false);
        assert!(sync.is_exportable());
        let mut fence = None;
        prepare(&sync, &mut fence, true, true).unwrap();
        assert!(fence.is_none());
        assert!(sync.is_reached());
        assert_eq!(state.waits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn interrupted_wait_prevents_submission() {
        let (sync, state) = point(false, true);
        assert_eq!(prepare(&sync, &mut None, true, true), Err(Interrupted));
        assert!(!sync.is_reached());
        assert_eq!(state.waits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_export_is_reused_without_waiting() {
        let (sync, state) = point(true, false);
        let mut fence = None;
        prepare(&sync, &mut fence, true, false).unwrap();
        let exported = fence.as_ref().unwrap().clone();
        prepare(&sync, &mut fence, true, true).unwrap();
        prepare(&sync, &mut fence, true, true).unwrap();
        assert!(Arc::ptr_eq(&exported, fence.as_ref().unwrap()));
        assert_eq!(state.exports.load(Ordering::SeqCst), 1);
        assert_eq!(state.waits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_only_does_not_wait_but_real_submission_does() {
        let (sync, state) = point(false, false);
        let mut fence = None;
        prepare(&sync, &mut fence, true, false).unwrap();
        assert_eq!(state.waits.load(Ordering::SeqCst), 0);
        prepare(&sync, &mut fence, false, true).unwrap();
        assert_eq!(state.waits.load(Ordering::SeqCst), 1);
        // The caller may also have already waited before reaching this point.
        prepare(&sync, &mut fence, false, true).unwrap();
        assert_eq!(state.waits.load(Ordering::SeqCst), 1);
    }
}
