/// Identifies the buffer whose pixels are described by the damage tracker's last render.
#[derive(Debug)]
pub(super) struct PrimaryBufferState<T> {
    rendered: Option<T>,
}

impl<T> Default for PrimaryBufferState<T> {
    fn default() -> Self {
        Self { rendered: None }
    }
}

impl<T: PartialEq> PrimaryBufferState<T> {
    pub(super) fn rendered(&mut self, buffer: T) {
        self.rendered = Some(buffer);
    }

    pub(super) fn invalidate(&mut self) {
        self.rendered = None;
    }

    pub(super) fn can_reuse(&self, previous: Option<&T>, allow_partial_update: bool) -> bool {
        allow_partial_update && self.rendered.is_some() && self.rendered.as_ref() == previous
    }
}

#[cfg(test)]
mod tests {
    use super::PrimaryBufferState;

    #[test]
    fn prepared_but_unsubmitted_frame_cannot_reuse_the_older_display_buffer() {
        let mut state = PrimaryBufferState::default();
        state.rendered(1);
        assert!(state.can_reuse(Some(&1), true));
        state.rendered(2);
        assert!(!state.can_reuse(Some(&1), true));
        assert!(state.can_reuse(Some(&2), true));
    }

    #[test]
    fn initial_direct_scanout_and_reset_states_require_rendering() {
        let mut state = PrimaryBufferState::default();
        assert!(!state.can_reuse(None, true));
        assert!(!state.can_reuse(Some(&1), true));
        state.rendered(1);
        assert!(!state.can_reuse(None, true));
        assert!(!state.can_reuse(Some(&1), false));
        state.invalidate();
        assert!(!state.can_reuse(Some(&1), true));
    }

    #[test]
    fn skipped_updates_do_not_change_the_rendered_buffer_identity() {
        let mut state = PrimaryBufferState::default();
        state.rendered(1);
        for _ in 0..10 {
            assert!(state.can_reuse(Some(&1), true));
        }
        assert!(!state.can_reuse(Some(&2), true));
    }
}
