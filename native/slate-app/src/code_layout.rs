//! How the Code view arranges its terminal cards.
//!
//! Layouts follow `src/renderer/src/lib/codeLayout.ts`.

use crate::canvas::{Pos2, Rect, Vec2};

/// `egui::pos2` shorthand kept so the arrangement code reads as before.
fn pos2(x: f32, y: f32) -> Pos2 {
    Pos2::new(x, y)
}

/// CodeView uses a contiguous grid (`gap-0`); only the three-way split has tracks.
const GAP: f32 = 0.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodeLayout {
    #[default]
    Auto,
    Grid,
    Columns,
    Rows,
    Focus,
}

impl CodeLayout {
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::Grid,
        Self::Columns,
        Self::Rows,
        Self::Focus,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Grid => "Grid",
            Self::Columns => "Columns",
            Self::Rows => "Rows",
            Self::Focus => "Focus",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Auto => "Built-in layout for the session count",
            Self::Grid => "Equal cards in a square grid",
            Self::Columns => "One column per terminal",
            Self::Rows => "One row per terminal",
            Self::Focus => "Last used terminal large, rest beside it",
        }
    }
}

/// Card rectangles in terminal order. `focus` is the card `Focus` enlarges;
/// out of range it falls back to the first, as the renderer's does.
pub fn arrange(mode: CodeLayout, area: Rect, count: usize, focus: usize) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![area];
    }
    match mode {
        CodeLayout::Auto => auto_layout(area, count),
        CodeLayout::Columns => (0..count)
            .map(|index| cell(area, count, 1, index, 0))
            .collect(),
        CodeLayout::Rows => (0..count)
            .map(|index| cell(area, 1, count, 0, index))
            .collect(),
        CodeLayout::Grid => {
            let columns = (count as f32).sqrt().ceil() as usize;
            let rows = count.div_ceil(columns);
            (0..count)
                .map(|index| cell(area, columns, rows, index % columns, index / columns))
                .collect()
        }
        CodeLayout::Focus => {
            let main = if focus < count { focus } else { 0 };
            let side = count - 1;
            let wide = (area.width() - GAP) * 2.0 / 3.0;
            let beside = Rect::from_min_max(pos2(area.left() + wide + GAP, area.top()), area.max);
            (0..count)
                .map(|index| {
                    if index == main {
                        Rect::from_min_size(area.min, Vec2::new(wide.max(1.0), area.height()))
                    } else {
                        let slot = if index < main { index } else { index - 1 };
                        cell(beside, 1, side, 0, slot)
                    }
                })
                .collect()
        }
    }
}

fn auto_layout(area: Rect, count: usize) -> Vec<Rect> {
    if count == 3 {
        return three_way(area, [0.5, 0.5]);
    }
    if count == 5 {
        let available = (area.width() - 2.0 * GAP).max(0.0);
        let narrow = available * 0.3;
        let mut cards = Vec::with_capacity(5);
        for index in 0..4 {
            let x = area.left() + (narrow + GAP) * (index % 2) as f32;
            let column = Rect::from_min_max(pos2(x, area.top()), pos2(x + narrow, area.bottom()));
            cards.push(cell(column, 1, 2, 0, index / 2));
        }
        cards.push(Rect::from_min_max(
            pos2(area.left() + 2.0 * (narrow + GAP), area.top()),
            area.max,
        ));
        return cards;
    }
    if (6..=20).contains(&count) {
        let rows = if count <= 8 || count == 10 {
            2
        } else if count <= 15 {
            3
        } else {
            4
        };
        let mut cards = Vec::with_capacity(count);
        for row in 0..rows {
            let in_row = count / rows + usize::from(row < count % rows);
            let strip = cell(area, 1, rows, 0, row);
            cards.extend((0..in_row).map(|column| cell(strip, in_row, 1, column, 0)));
        }
        return cards;
    }
    let columns = if count <= 1 {
        1
    } else if count <= 4 {
        2
    } else {
        4
    };
    (0..count)
        .map(|index| {
            cell(
                area,
                columns,
                count.div_ceil(columns),
                index % columns,
                index / columns,
            )
        })
        .collect()
}

pub fn three_way(area: Rect, split: [f32; 2]) -> Vec<Rect> {
    let gap = 2.0 * GAP + 2.0;
    let ratio = |value: f32| {
        if value.is_finite() {
            value.clamp(0.2, 0.8)
        } else {
            0.5
        }
    };
    let x = area.left() + (area.width() - gap).max(0.0) * ratio(split[0]);
    let y = area.top() + (area.height() - gap).max(0.0) * ratio(split[1]);
    vec![
        Rect::from_min_max(area.min, pos2(x, area.bottom())),
        Rect::from_min_max(pos2(x + gap, area.top()), pos2(area.right(), y)),
        Rect::from_min_max(pos2(x + gap, y + gap), area.max),
    ]
}

/// Edges are taken as a fraction of the whole rather than accumulated from a
/// card width, so rounding cannot walk the far edge past the area — and the
/// clamp keeps that true even where the division is not exact.
fn cell(area: Rect, columns: usize, rows: usize, column: usize, row: usize) -> Rect {
    let x = |index: usize| area.left() + (area.width() + GAP) * index as f32 / columns as f32;
    let y = |index: usize| area.top() + (area.height() + GAP) * index as f32 / rows as f32;
    Rect::from_min_max(
        pos2(x(column), y(row)),
        pos2(x(column + 1) - GAP, y(row + 1) - GAP),
    )
    .intersect(area)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(1000.0, 500.0))
    }

    #[test]
    fn one_terminal_fills_the_area_in_every_mode() {
        for mode in CodeLayout::ALL {
            assert_eq!(arrange(mode, area(), 1, 0), vec![area()]);
        }
    }

    #[test]
    fn cards_never_overlap_and_stay_inside_the_area() {
        for mode in CodeLayout::ALL {
            for count in 2..=24 {
                let cards = arrange(mode, area(), count, 0);
                assert_eq!(cards.len(), count);
                for (index, card) in cards.iter().enumerate() {
                    assert!(
                        area().contains_rect(*card),
                        "{mode:?}/{count} card {index} escapes"
                    );
                    for other in &cards[index + 1..] {
                        assert!(
                            !card.intersect(*other).is_positive(),
                            "{mode:?}/{count} cards overlap"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn columns_and_rows_run_one_deep() {
        let columns = arrange(CodeLayout::Columns, area(), 4, 0);
        assert!(columns.iter().all(|card| card.height() == area().height()));
        let rows = arrange(CodeLayout::Rows, area(), 4, 0);
        assert!(rows.iter().all(|card| card.width() == area().width()));
    }

    #[test]
    fn auto_uses_the_electron_five_card_and_dense_shapes() {
        let five = arrange(CodeLayout::Auto, area(), 5, 0);
        assert_eq!(five[4].height(), area().height());
        assert!((five[4].width() / five[0].width() - 4.0 / 3.0).abs() < 0.001);
        for (count, expected) in [
            (7, vec![4, 3]),
            (9, vec![3, 3, 3]),
            (10, vec![5, 5]),
            (16, vec![4, 4, 4, 4]),
        ] {
            let cards = arrange(CodeLayout::Auto, area(), count, 0);
            let mut rows: Vec<usize> = Vec::new();
            let mut top = f32::NEG_INFINITY;
            for card in cards {
                if card.top() != top {
                    rows.push(0);
                    top = card.top();
                }
                *rows.last_mut().unwrap() += 1;
            }
            assert_eq!(rows, expected);
        }
    }

    #[test]
    fn three_way_clamps_invalid_ratios_and_preserves_separator_space() {
        assert_eq!(
            three_way(area(), [f32::NAN, f32::INFINITY]),
            three_way(area(), [0.5, 0.5])
        );
        let cards = three_way(area(), [-1.0, 2.0]);
        assert_eq!(cards[1].left() - cards[0].right(), 2.0);
        assert_eq!(cards[2].top() - cards[1].bottom(), 2.0);
        assert!(cards.iter().all(|card| area().contains_rect(*card)));
    }

    #[test]
    fn focus_enlarges_the_chosen_card_and_stacks_the_rest() {
        let cards = arrange(CodeLayout::Focus, area(), 3, 1);
        assert_eq!(cards[1].height(), area().height());
        assert!(cards[1].width() > cards[0].width());
        assert!(cards[0].top() < cards[2].top());
    }

    /// An out-of-range focus must not panic or drop a card.
    #[test]
    fn focus_out_of_range_falls_back_to_the_first_card() {
        assert_eq!(
            arrange(CodeLayout::Focus, area(), 3, 99),
            arrange(CodeLayout::Focus, area(), 3, 0)
        );
    }
}
