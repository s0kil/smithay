use crate::utils::{Buffer, Rectangle};

/// Clip damage and preserve the existing ordered, single-pass merge policy
/// without allocating two replacement vectors for every rectangle.
pub(super) fn merge_damage(
    damage: &[Rectangle<i32, Buffer>],
    bounds: Rectangle<i32, Buffer>,
) -> Vec<Rectangle<i32, Buffer>> {
    merge_damage_iter(damage.iter().filter_map(|rect| rect.intersection(bounds)))
}

/// Merge already validated output-copy rectangles without changing their bounds.
pub(super) fn merge_damage_unclipped(damage: &[Rectangle<i32, Buffer>]) -> Vec<Rectangle<i32, Buffer>> {
    merge_damage_iter(damage.iter().copied())
}

fn merge_damage_iter(
    damage: impl IntoIterator<Item = Rectangle<i32, Buffer>>,
) -> Vec<Rectangle<i32, Buffer>> {
    let mut merged = Vec::<Rectangle<i32, Buffer>>::new();
    for mut rect in damage {
        // Select against the incoming rectangle, not its growing union, to
        // retain the old merge order and avoid expanding unrelated copies.
        let incoming = rect;
        merged.retain(|other| {
            if other.overlaps_or_touches(incoming) {
                rect = rect.merge(*other);
                false
            } else {
                true
            }
        });
        merged.push(rect);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Buffer> {
        Rectangle::new((x, y).into(), (width, height).into())
    }

    #[test]
    fn clips_outside_damage_and_merges_touching_regions() {
        let bounds = rect(0, 0, 100, 100);
        assert_eq!(
            merge_damage(
                &[
                    rect(-5, -5, 10, 10),
                    rect(5, 5, 5, 5),
                    rect(90, 90, 20, 20),
                    rect(150, 0, 10, 10)
                ],
                bounds
            ),
            vec![rect(0, 0, 10, 10), rect(90, 90, 10, 10)],
        );
        assert!(merge_damage(&[], bounds).is_empty());
    }

    #[test]
    fn preserves_previous_ordered_merge_results() {
        let bounds = rect(0, 0, 100, 100);
        let candidates = [
            rect(-10, -10, 20, 20),
            rect(5, 5, 20, 20),
            rect(30, 30, 5, 5),
            rect(10, 10, 25, 25),
            rect(70, 80, 40, 40),
            rect(101, 101, 10, 10),
            rect(0, 0, 100, 100),
            rect(80, -10, 10, 20),
        ];
        for mask in 0..(1 << candidates.len()) {
            let damage = candidates
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, rect)| *rect)
                .collect::<Vec<_>>();
            let previous = damage.iter().filter_map(|r| r.intersection(bounds)).fold(
                Vec::<Rectangle<i32, Buffer>>::new(),
                |damage, mut rect| {
                    let (overlapping, mut remaining): (Vec<_>, Vec<_>) = damage
                        .into_iter()
                        .partition(|other| other.overlaps_or_touches(rect));
                    for overlap in overlapping {
                        rect = rect.merge(overlap);
                    }
                    remaining.push(rect);
                    remaining
                },
            );
            assert_eq!(merge_damage(&damage, bounds), previous, "damage subset {mask}");
        }
    }

    #[test]
    fn output_copy_keeps_unclipped_bounds_and_previous_merge_order() {
        let candidates = [
            rect(-10, -10, 20, 20),
            rect(5, 5, 20, 20),
            rect(30, 30, 5, 5),
            rect(10, 10, 25, 25),
            rect(90, 90, 20, 20),
            rect(100, 0, 10, 10),
        ];
        for mask in 0..(1 << candidates.len()) {
            let damage = candidates
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, rect)| *rect)
                .collect::<Vec<_>>();
            let previous = damage.iter().copied().fold(Vec::new(), |damage, mut rect| {
                let (overlapping, mut remaining): (Vec<_>, Vec<_>) = damage
                    .into_iter()
                    .partition(|other: &Rectangle<i32, Buffer>| other.overlaps_or_touches(rect));
                for overlap in overlapping {
                    rect = rect.merge(overlap);
                }
                remaining.push(rect);
                remaining
            });
            assert_eq!(merge_damage_unclipped(&damage), previous, "subset {mask}");
        }
    }
}
