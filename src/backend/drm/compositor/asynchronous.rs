//! Submit an optional async flip, retaining the actual mode accepted by KMS.

use std::time::{Duration, Instant};

use drm::control::PageFlipFlags;

use crate::backend::{drm::DrmError, renderer::PresentationMode};

/// Preserve renderer compatibility and advertised format/modifier pairs.
/// An absent property is unknown support; an empty property means no support.
pub(super) fn compatible_modifiers(
    normal: &[drm_fourcc::DrmModifier],
    code: drm_fourcc::DrmFourcc,
    advertised: Option<&crate::backend::allocator::format::FormatSet>,
) -> Vec<drm_fourcc::DrmModifier> {
    normal
        .iter()
        .copied()
        .filter(|modifier| {
            advertised.is_none_or(|formats| {
                formats.contains(&drm_fourcc::DrmFormat {
                    code,
                    modifier: *modifier,
                })
            })
        })
        .collect()
}

pub(super) fn submit(
    eligible: bool,
    retry_after: &mut Option<Instant>,
    now: Instant,
    mut flip: impl FnMut(PageFlipFlags) -> Result<(), DrmError>,
) -> Result<PresentationMode, DrmError> {
    if !eligible || retry_after.is_some_and(|deadline| now < deadline) {
        flip(PageFlipFlags::EVENT)?;
        return Ok(PresentationMode::VSync);
    }
    match flip(PageFlipFlags::EVENT | PageFlipFlags::ASYNC) {
        Ok(()) => {
            *retry_after = None;
            Ok(PresentationMode::Async)
        }
        Err(error) => {
            // A driver may advertise async flips but reject this configuration.
            // Avoid an ioctl failure per frame, while allowing eventual recovery
            // without a modeset. Transient failures must not poison the cache.
            if matches!(&error, DrmError::Access(access) if matches!(access.source.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported))
            {
                *retry_after = Some(now + Duration::from_secs(1));
            }
            tracing::debug!(?error, "Async flip rejected; retrying synchronized");
            flip(PageFlipFlags::EVENT)?;
            Ok(PresentationMode::VSync)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::drm::error::AccessError;

    #[test]
    fn modifier_preference_respects_both_renderer_and_scanout_format() {
        use drm_fourcc::{DrmFormat, DrmFourcc, DrmModifier};
        let normal = [DrmModifier::Linear, DrmModifier::Invalid];
        let formats = [
            DrmFormat {
                code: DrmFourcc::Xrgb8888,
                modifier: DrmModifier::Linear,
            },
            DrmFormat {
                code: DrmFourcc::Argb8888,
                modifier: DrmModifier::Invalid,
            },
        ]
        .into_iter()
        .collect();
        assert_eq!(
            compatible_modifiers(&normal, DrmFourcc::Xrgb8888, Some(&formats)),
            vec![DrmModifier::Linear]
        );
        assert_eq!(
            compatible_modifiers(&normal, DrmFourcc::Argb8888, Some(&formats)),
            vec![DrmModifier::Invalid]
        );
        assert_eq!(compatible_modifiers(&normal, DrmFourcc::Xrgb8888, None), normal);
        assert!(compatible_modifiers(&normal, DrmFourcc::Xrgb8888, Some(&Default::default())).is_empty());
        assert!(
            compatible_modifiers(&[DrmModifier::Invalid], DrmFourcc::Xrgb8888, Some(&formats)).is_empty()
        );
    }

    fn error(kind: std::io::ErrorKind) -> DrmError {
        DrmError::Access(AccessError {
            errmsg: "test flip failure",
            dev: None,
            source: std::io::Error::from(kind),
        })
    }

    #[test]
    fn reports_accepted_async_mode_and_requests_completion() {
        let mut retry = None;
        let mode = submit(true, &mut retry, Instant::now(), |flags| {
            assert_eq!(flags, PageFlipFlags::EVENT | PageFlipFlags::ASYNC);
            Ok(())
        })
        .unwrap();
        assert_eq!(mode, PresentationMode::Async);
    }

    #[test]
    fn rejection_falls_back_then_retries_after_cooldown() {
        let now = Instant::now();
        let mut retry = None;
        let mut attempts = 0;
        let mode = submit(true, &mut retry, now, |flags| {
            attempts += 1;
            if flags.contains(PageFlipFlags::ASYNC) {
                Err(error(std::io::ErrorKind::InvalidInput))
            } else {
                assert_eq!(flags, PageFlipFlags::EVENT);
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(mode, PresentationMode::VSync);
        assert_eq!(
            submit(true, &mut retry, now, |flags| {
                assert_eq!(flags, PageFlipFlags::EVENT);
                Ok(())
            })
            .unwrap(),
            PresentationMode::VSync
        );
        assert_eq!(
            submit(true, &mut retry, now + Duration::from_secs(1), |flags| {
                assert!(flags.contains(PageFlipFlags::ASYNC));
                Ok(())
            })
            .unwrap(),
            PresentationMode::Async
        );
        assert!(retry.is_none());
    }

    #[test]
    fn transient_failure_does_not_disable_async() {
        let mut retry = None;
        submit(true, &mut retry, Instant::now(), |flags| {
            if flags.contains(PageFlipFlags::ASYNC) {
                Err(error(std::io::ErrorKind::WouldBlock))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert!(retry.is_none());
    }

    #[test]
    fn propagates_failed_fallback_and_never_tears_ineligible_frames() {
        assert!(
            submit(true, &mut None, Instant::now(), |_| {
                Err(error(std::io::ErrorKind::PermissionDenied))
            })
            .is_err()
        );
        assert_eq!(
            submit(false, &mut None, Instant::now(), |flags| {
                assert_eq!(flags, PageFlipFlags::EVENT);
                Ok(())
            })
            .unwrap(),
            PresentationMode::VSync
        );
    }
}
