//! The Commits tab occupies one column, with independently scrollable stacked panes.
//! The split itself, a proportion of the rows given to the upper pane, is shared with
//! the Files tab, which stacks the outline under the tree the same way.
use helix_view::graphics::Rect;

use crate::ui::panel_width;

const STATE_FILE: &str = "sidebar-commits";
const DEFAULT_MAX_WIDTH: u16 = 110;
const MIN_PANE_ROWS: u16 = 3;

/// Two panes stacked in `area`, the upper one given `share` thousandths of the usable
/// rows. Both rectangles include a heading: the tab strip above and the draggable rule
/// below. Tiny terminals show the focused list alone until both panes fit again.
pub fn stacked_panes(area: Rect, share: u16) -> Option<[Rect; 2]> {
    if area.height < 4 {
        return None;
    }
    let rows = area.height - 2;
    let min = MIN_PANE_ROWS.min(rows / 2);
    let upper = (u32::from(rows) * u32::from(share) / 1000) as u16;
    let upper = upper.clamp(min, rows - min);
    let top = Rect::new(area.x, area.y, area.width, upper + 1);
    let bottom = Rect::new(area.x, top.bottom(), area.width, area.height - top.height);
    Some([top, bottom])
}

/// The narrowest either pane may be dragged to when they sit side by side.
const MIN_PANE_COLS: u16 = 8;

/// Two panes side by side in `area`, the left one given `share` thousandths of the usable
/// columns. Each keeps a column at its right edge for a rule: the left pane the draggable
/// one between them, the right pane the sidebar's own separator. None when the area is too
/// narrow for both.
pub fn side_panes(area: Rect, share: u16) -> Option<[Rect; 2]> {
    let columns = area.width.checked_sub(2)?;
    if columns < MIN_PANE_COLS * 2 {
        return None;
    }
    let left = (u32::from(columns) * u32::from(share) / 1000) as u16;
    let left = left.clamp(MIN_PANE_COLS, columns - MIN_PANE_COLS);
    let first = Rect::new(area.x, area.y, left + 1, area.height);
    let second = Rect::new(first.right(), area.y, area.width - first.width, area.height);
    Some([first, second])
}

/// The share that puts the rule between two side by side panes on screen column `column`;
/// none when the area is too narrow to split.
pub fn share_at_column(area: Rect, column: u16) -> Option<u16> {
    let columns = area.width.checked_sub(2)?;
    if columns < MIN_PANE_COLS * 2 {
        return None;
    }
    let left = column
        .saturating_sub(area.x)
        .clamp(MIN_PANE_COLS, columns - MIN_PANE_COLS);
    // Round up so converting the share back to columns lands exactly on the pointer.
    Some((u32::from(left) * 1000).div_ceil(u32::from(columns)) as u16)
}

/// The share that puts the rule between the panes on screen row `row`; none when the
/// area is too small to split.
pub fn share_at(area: Rect, row: u16) -> Option<u16> {
    if area.height < 4 {
        return None;
    }
    let rows = area.height - 2;
    let min = MIN_PANE_ROWS.min(rows / 2);
    let upper = row
        .saturating_sub(area.y)
        .saturating_sub(1)
        .clamp(min, rows - min);
    // Round up so converting the share back to rows lands exactly on the pointer.
    Some((u32::from(upper) * 1000).div_ceil(u32::from(rows)) as u16)
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CommitLayout {
    pub width: Option<u16>,
    /// Proportion of the usable rows assigned to history, in thousandths. A proportion
    /// follows terminal height changes without stranding a fixed-size pane off screen.
    history_share: u16,
    /// The columns the history keeps when the panes sit side by side, with the sidebar
    /// across the top of the screen; thousandths too.
    history_columns: u16,
}

impl Default for CommitLayout {
    fn default() -> Self {
        Self {
            width: None,
            history_share: 667,
            history_columns: 600,
        }
    }
}

impl CommitLayout {
    pub fn load() -> anyhow::Result<Self> {
        let layout: Option<Self> = panel_width::load_state(STATE_FILE)?;
        let layout = layout.unwrap_or_default();
        anyhow::ensure!(
            layout.history_share <= 1000 && layout.history_columns <= 1000,
            "invalid commit pane proportion"
        );
        Ok(layout)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        panel_width::save_state(STATE_FILE, self)
    }

    pub fn width(&self, screen_width: u16) -> u16 {
        self.width
            .unwrap_or((screen_width / 2).min(DEFAULT_MAX_WIDTH))
            .max(super::MIN_WIDTH)
    }

    /// The history over the files, or with `side` the files beside the history.
    pub fn panes(&self, area: Rect, side: bool) -> Option<[Rect; 2]> {
        if side {
            side_panes(area, self.history_columns)
        } else {
            stacked_panes(area, self.history_share)
        }
    }

    /// Drags the rule between the panes to the pointer: the row it is on when they are
    /// stacked, the column when they sit side by side.
    pub fn resize_split(&mut self, area: Rect, row: u16, column: u16, side: bool) {
        if side {
            if let Some(share) = share_at_column(area, column) {
                self.history_columns = share;
            }
        } else if let Some(share) = share_at(area, row) {
            self.history_share = share;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_column_uses_half_a_small_screen_but_leaves_wide_screens_to_code() {
        let layout = CommitLayout::default();
        assert_eq!(layout.width(120), 60);
        assert_eq!(layout.width(160), 80);
        assert_eq!(layout.width(220), 110);
        assert_eq!(layout.width(300), 110);
    }

    #[test]
    fn stacked_panes_cover_the_area_and_keep_their_minimum_when_dragged() {
        let mut layout = CommitLayout::default();
        let area = Rect::new(4, 5, 80, 41);
        let [top, bottom] = layout.panes(area, false).unwrap();
        assert_eq!(top.height - 1, 26);
        assert_eq!(bottom.height - 1, 13);
        assert_eq!(top.bottom(), bottom.y);
        assert_eq!(bottom.bottom(), area.bottom());
        layout.resize_split(area, 22, 0, false);
        assert_eq!(layout.panes(area, false).unwrap()[1].y, 22);
        layout.resize_split(area, 0, 0, false);
        assert_eq!(layout.panes(area, false).unwrap()[0].height - 1, 3);
        layout.resize_split(area, u16::MAX, 0, false);
        assert_eq!(layout.panes(area, false).unwrap()[1].height - 1, 3);
        assert!(layout.panes(area.with_height(3), false).is_none());
        // Side by side the same rule is a column, dragged by its own share.
        let [history, files] = layout.panes(area, true).unwrap();
        assert_eq!(history.right(), files.x);
        assert_eq!(files.right(), area.right());
        layout.resize_split(area, 0, 30, true);
        assert_eq!(layout.panes(area, true).unwrap()[1].x, 31);
        assert_eq!(layout.panes(area, false).unwrap()[1].height - 1, 3);
    }

    #[test]
    fn saved_proportion_survives_a_resize_without_consuming_either_pane() {
        let mut layout = CommitLayout::default();
        let area = Rect::new(0, 0, 80, 42);
        layout.resize_split(area, 21, 0, false);
        layout.width = Some(65);
        let saved = toml::to_string(&layout).unwrap();
        let restored: CommitLayout = toml::from_str(&saved).unwrap();
        assert_eq!(restored.width(300), 65);
        let [top, bottom] = restored.panes(area.with_height(82), false).unwrap();
        assert_eq!(top.height, bottom.height);
        for height in 4..120 {
            let area = area.with_height(height);
            let [top, bottom] = restored.panes(area, false).unwrap();
            assert!(top.height >= 2 && bottom.height >= 2);
            assert_eq!(top.bottom(), bottom.y);
            assert_eq!(bottom.bottom(), area.bottom());
        }
    }
}
