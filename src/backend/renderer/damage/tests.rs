use std::{cell::Cell, rc::Rc};

use super::*;
use crate::{
    backend::renderer::{
        element::{Id, RenderElement},
        test::{DummyError, DummyFrame, DummyFramebuffer, DummyRenderer},
        utils::{CommitCounter, OpaqueRegions},
    },
    utils::{Buffer, Scale},
};

struct TestElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<i32, Physical>,
    draws: Rc<Cell<usize>>,
    fail: bool,
    alpha: f32,
    opaque: bool,
}

impl TestElement {
    fn new() -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            geometry: Rectangle::new((10, 10).into(), (20, 20).into()),
            draws: Rc::new(Cell::new(0)),
            fail: false,
            alpha: 1.0,
            opaque: false,
        }
    }
}

impl Element for TestElement {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size((self.geometry.size.w as f64, self.geometry.size.h as f64).into())
    }
    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }
    fn alpha(&self) -> f32 {
        self.alpha
    }
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        if self.opaque {
            OpaqueRegions::from_slice(&[Rectangle::from_size(self.geometry.size)])
        } else {
            OpaqueRegions::default()
        }
    }
}

impl RenderElement<DummyRenderer> for TestElement {
    fn draw(
        &self,
        _frame: &mut DummyFrame,
        _src: Rectangle<f64, Buffer>,
        _dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&crate::utils::user_data::UserDataMap>,
    ) -> Result<(), DummyError> {
        self.draws.set(self.draws.get() + 1);
        if self.fail {
            Err(DummyError::SyncInterrupted)
        } else {
            Ok(())
        }
    }
}

fn tracker() -> OutputDamageTracker {
    OutputDamageTracker::new((100, 100), 1.0, Transform::Normal)
}

fn render(
    tracker: &mut OutputDamageTracker,
    element: &TestElement,
    age: usize,
    only_changes: bool,
) -> Option<Vec<Rectangle<i32, Physical>>> {
    let mut renderer = DummyRenderer;
    let mut framebuffer = DummyFramebuffer;
    let elements = std::slice::from_ref(element);
    let result = if only_changes {
        tracker.render_output_if_changed(&mut renderer, &mut framebuffer, age, elements, Color32F::BLACK)
    } else {
        tracker.render_output(&mut renderer, &mut framebuffer, age, elements, Color32F::BLACK)
    }
    .unwrap();
    result.damage.cloned()
}

#[test]
fn unchanged_scene_does_not_repair_or_advance_an_unused_back_buffer() {
    let mut tracker = tracker();
    let mut element = TestElement::new();
    assert!(render(&mut tracker, &element, 0, true).is_some());
    element.commit.increment();
    assert!(render(&mut tracker, &element, 0, true).is_some());
    let old_damage = tracker.last_state.old_damage.clone();
    for age in [2, 3, 0, 2] {
        assert!(render(&mut tracker, &element, age, true).is_none());
        assert_eq!(tracker.last_state.old_damage, old_damage);
    }
    assert_eq!(element.draws.get(), 2);
}

#[test]
fn ordinary_rendering_still_repairs_an_aged_buffer() {
    let mut tracker = tracker();
    let mut element = TestElement::new();
    render(&mut tracker, &element, 0, false).unwrap();
    element.commit.increment();
    render(&mut tracker, &element, 0, false).unwrap();
    assert!(render(&mut tracker, &element, 2, false).is_some());
    assert_eq!(element.draws.get(), 3);
}

#[test]
fn new_damage_after_skipped_frames_includes_buffer_age_repair() {
    let mut tracker = tracker();
    let mut element = TestElement::new();
    render(&mut tracker, &element, 0, true).unwrap();
    element.geometry.loc = (40, 40).into();
    element.commit.increment();
    render(&mut tracker, &element, 0, true).unwrap();
    for _ in 0..10 {
        assert!(render(&mut tracker, &element, 2, true).is_none());
    }
    element.geometry.loc = (20, 50).into();
    element.commit.increment();
    let damage = render(&mut tracker, &element, 2, true).unwrap();
    // The old position only occurs in the previous frame's damage. Skipped frames must
    // not age that history out before the older back buffer is actually used again.
    assert!(damage.iter().any(|r| r.contains((10, 10))));
    assert!(damage.iter().any(|r| r.contains((20, 50))));
    assert_eq!(element.draws.get(), 3);
}

#[test]
fn geometry_changes_redraw_without_a_new_buffer_commit() {
    let mut tracker = tracker();
    let mut element = TestElement::new();
    render(&mut tracker, &element, 0, true).unwrap();
    element.geometry.loc = (40, 40).into();
    assert!(render(&mut tracker, &element, 1, true).is_some());
    assert_eq!(element.draws.get(), 2);
}

#[test]
fn clear_color_changes_and_element_removal_redraw() {
    let mut tracker = tracker();
    let element = TestElement::new();
    render(&mut tracker, &element, 0, true).unwrap();
    let mut renderer = DummyRenderer;
    let mut framebuffer = DummyFramebuffer;
    assert!(
        tracker
            .render_output_if_changed(
                &mut renderer,
                &mut framebuffer,
                1,
                std::slice::from_ref(&element),
                Color32F::TRANSPARENT
            )
            .unwrap()
            .damage
            .is_some()
    );
    assert!(
        tracker
            .render_output_if_changed::<TestElement, _>(
                &mut renderer,
                &mut framebuffer,
                1,
                &[],
                Color32F::TRANSPARENT
            )
            .unwrap()
            .damage
            .is_some()
    );
}

#[test]
fn output_resize_forces_rendering_even_without_surface_changes() {
    let mut tracker = tracker();
    let element = TestElement::new();
    render(&mut tracker, &element, 0, true).unwrap();
    tracker.mode = OutputModeSource::Static {
        size: (200, 100).into(),
        scale: 1.0.into(),
        transform: Transform::Normal,
    };
    assert!(render(&mut tracker, &element, 2, true).is_some());
    assert_eq!(element.draws.get(), 2);
}

#[test]
fn failed_render_does_not_make_the_retry_look_unchanged() {
    let mut tracker = tracker();
    let mut element = TestElement::new();
    render(&mut tracker, &element, 0, true).unwrap();
    element.commit.increment();
    element.fail = true;
    let result = tracker.render_output_if_changed(
        &mut DummyRenderer,
        &mut DummyFramebuffer,
        2,
        std::slice::from_ref(&element),
        Color32F::BLACK,
    );
    assert!(result.is_err());
    element.fail = false;
    assert!(render(&mut tracker, &element, 2, true).is_some());
}

#[test]
fn unchanged_scene_does_not_bind_a_framebuffer_or_advance_history() {
    let mut tracker = tracker();
    let element = TestElement::new();
    let binds = Cell::new(0);
    let draw = |tracker: &mut OutputDamageTracker, age| {
        tracker
            .render_output_if_changed_with_bind(
                &mut DummyRenderer,
                |_| {
                    binds.set(binds.get() + 1);
                    Ok(DummyFramebuffer)
                },
                age,
                std::slice::from_ref(&element),
                Color32F::BLACK,
            )
            .unwrap()
            .damage
            .is_some()
    };
    assert!(draw(&mut tracker, 0));
    let history = tracker.last_state.old_damage.clone();
    for age in [0, 1, 2, 4] {
        assert!(!draw(&mut tracker, age));
        assert_eq!(tracker.last_state.old_damage, history);
    }
    assert_eq!(binds.get(), 1);
    assert_eq!(element.draws.get(), 1);
}

#[test]
fn failed_lazy_bind_forces_a_full_redraw_on_retry() {
    let mut tracker = tracker();
    let mut element = TestElement::new();
    render(&mut tracker, &element, 0, true).unwrap();
    element.commit.increment();
    let result = tracker.render_output_if_changed_with_bind(
        &mut DummyRenderer,
        |_| Err(DummyError::SyncInterrupted),
        1,
        std::slice::from_ref(&element),
        Color32F::BLACK,
    );
    assert!(result.is_err());
    let damage = render(&mut tracker, &element, 1, true).unwrap();
    assert!(damage.iter().any(|r| r.contains((99, 99))));
}

#[test]
fn alpha_changes_redraw_without_a_new_buffer_commit() {
    let mut tracker = tracker();
    let mut element = TestElement::new();
    render(&mut tracker, &element, 0, true).unwrap();
    element.alpha = 0.5;
    assert!(render(&mut tracker, &element, 2, true).is_some());
    assert_eq!(element.draws.get(), 2);
}

#[test]
fn hidden_surface_commits_are_drawn_when_the_surface_is_revealed() {
    let mut tracker = tracker();
    let mut elements = [TestElement::new(), TestElement::new()];
    elements[0].geometry = Rectangle::from_size((100, 100).into());
    elements[0].opaque = true;
    let draw = |tracker: &mut OutputDamageTracker, elements: &[TestElement]| {
        tracker
            .render_output_if_changed(
                &mut DummyRenderer,
                &mut DummyFramebuffer,
                1,
                elements,
                Color32F::BLACK,
            )
            .unwrap()
            .damage
            .cloned()
    };
    assert!(draw(&mut tracker, &elements).is_some());
    assert_eq!(elements[1].draws.get(), 0);
    elements[1].commit.increment();
    assert!(draw(&mut tracker, &elements).is_none());
    assert_eq!(elements[1].draws.get(), 0);
    elements[0].geometry.loc = (50, 50).into();
    let damage = draw(&mut tracker, &elements).unwrap();
    assert!(damage.iter().any(|rect| rect.contains((10, 10))));
    assert_eq!(elements[1].draws.get(), 1);
}

#[test]
fn shrinking_opaque_coverage_redraws_without_a_buffer_commit() {
    let mut tracker = tracker();
    let mut element = TestElement::new();
    element.opaque = true;
    render(&mut tracker, &element, 0, true).unwrap();
    assert!(render(&mut tracker, &element, 1, true).is_none());
    element.opaque = false;
    let damage = render(&mut tracker, &element, 1, true).unwrap();
    assert!(damage.iter().any(|rect| rect.contains((10, 10))));
    assert_eq!(element.draws.get(), 2);
}

#[test]
fn output_transform_change_redraws_an_unchanged_scene() {
    let mut tracker = tracker();
    let element = TestElement::new();
    render(&mut tracker, &element, 0, true).unwrap();
    assert!(render(&mut tracker, &element, 1, true).is_none());
    tracker.mode = OutputModeSource::Static {
        size: (100, 100).into(),
        scale: 1.0.into(),
        transform: Transform::_180,
    };
    assert!(render(&mut tracker, &element, 1, true).is_some());
    assert_eq!(element.draws.get(), 2);
}
