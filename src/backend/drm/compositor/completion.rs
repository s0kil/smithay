use crate::backend::renderer::PresentationMode;

use super::FrameSubmitted;

/// Keep completion and the next submission independent, including on error.
pub(super) fn complete_frame<S, F, U, E>(
    state: &mut S,
    take_pending: impl FnOnce(&mut S) -> Option<(F, U, PresentationMode)>,
    promote: impl FnOnce(&mut S, F) -> F,
    submit_next: impl FnOnce(&mut S) -> Result<(), E>,
) -> Option<FrameSubmitted<U, E>> {
    let (frame, user_data, presentation_mode) = take_pending(state)?;
    // Keep the retired frame's buffers and plane claims alive through submission,
    // matching the lifetime of the old mem::swap-based completion path.
    let retired_frame = promote(state, frame);
    let next_frame_result = submit_next(state);
    drop(retired_frame);
    Some(FrameSubmitted {
        user_data,
        presentation_mode,
        next_frame_result,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct State {
        current: u8,
        pending: Option<(u8, &'static str, PresentationMode)>,
        queued: Option<u8>,
        submits: usize,
    }

    fn complete(state: &mut State, fail: bool) -> Option<FrameSubmitted<&'static str, &'static str>> {
        complete_frame(
            state,
            |state| state.pending.take(),
            |state, frame| std::mem::replace(&mut state.current, frame),
            |state| {
                assert_eq!(state.current, 1, "promote A before submitting B");
                let Some(frame) = state.queued.take() else {
                    return Ok(());
                };
                state.submits += 1;
                if fail {
                    Err("B failed")
                } else {
                    state.pending = Some((frame, "B", PresentationMode::VSync));
                    Ok(())
                }
            },
        )
    }

    fn initial() -> State {
        State {
            pending: Some((1, "A", PresentationMode::Async)),
            queued: Some(2),
            ..Default::default()
        }
    }

    #[test]
    fn successor_failure_preserves_completed_metadata_exactly_once() {
        let mut state = initial();
        let completed = complete(&mut state, true).unwrap();
        assert_eq!(completed.user_data, "A");
        assert_eq!(completed.presentation_mode, PresentationMode::Async);
        assert_eq!(completed.next_frame_result, Err("B failed"));
        assert_eq!(state.current, 1);
        assert!(state.pending.is_none());
        assert!(state.queued.is_none());
        assert!(complete(&mut state, false).is_none());
        assert_eq!(state.submits, 1);
    }

    #[test]
    fn successor_success_does_not_replace_completed_mode() {
        let mut state = initial();
        let completed = complete(&mut state, false).unwrap();
        assert_eq!(completed.user_data, "A");
        assert_eq!(completed.presentation_mode, PresentationMode::Async);
        assert_eq!(completed.next_frame_result, Ok(()));
        assert_eq!(state.pending, Some((2, "B", PresentationMode::VSync)));
    }

    #[test]
    fn no_pending_frame_does_not_consume_a_queued_frame() {
        let mut state = State {
            queued: Some(2),
            ..Default::default()
        };
        assert!(complete(&mut state, false).is_none());
        assert_eq!(state.queued, Some(2));
        assert_eq!(state.submits, 0);
    }

    #[test]
    fn retired_frame_lives_through_successful_and_failed_successor_submission() {
        use std::{cell::RefCell, rc::Rc};

        struct Frame {
            name: &'static str,
            events: Rc<RefCell<Vec<&'static str>>>,
        }
        impl Drop for Frame {
            fn drop(&mut self) {
                self.events.borrow_mut().push(self.name);
            }
        }
        struct Frames {
            current: Frame,
            pending: Option<Frame>,
        }

        for fail in [false, true] {
            let events = Rc::new(RefCell::new(Vec::new()));
            let mut frames = Frames {
                current: Frame {
                    name: "retired",
                    events: events.clone(),
                },
                pending: Some(Frame {
                    name: "completed",
                    events: events.clone(),
                }),
            };
            let completed = complete_frame(
                &mut frames,
                |frames| {
                    frames
                        .pending
                        .take()
                        .map(|frame| (frame, (), PresentationMode::Async))
                },
                |frames, frame| std::mem::replace(&mut frames.current, frame),
                |frames| {
                    assert!(
                        events.borrow().is_empty(),
                        "retired resources released before submission"
                    );
                    assert_eq!(frames.current.name, "completed");
                    events.borrow_mut().push("submit");
                    if fail { Err(()) } else { Ok(()) }
                },
            )
            .unwrap();
            assert_eq!(completed.next_frame_result.is_err(), fail);
            assert_eq!(*events.borrow(), ["submit", "retired"]);
        }
    }
}
