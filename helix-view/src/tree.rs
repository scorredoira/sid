use crate::{graphics::Rect, View, ViewId};
use slotmap::SlotMap;

// the dimensions are recomputed on window resize/tree change.
//
#[derive(Debug)]
pub struct Tree {
    root: ViewId,
    // (container, index inside the container)
    pub focus: ViewId,
    // fullscreen: bool,
    area: Rect,

    nodes: SlotMap<ViewId, Node>,

    // used for traversals
    stack: Vec<(ViewId, Rect)>,
}

#[derive(Debug)]
pub struct Node {
    parent: ViewId,
    content: Content,
}

#[derive(Debug)]
pub enum Content {
    View(Box<View>),
    Container(Box<Container>),
}

impl Node {
    pub fn container(layout: Layout) -> Self {
        Self {
            parent: ViewId::default(),
            content: Content::Container(Box::new(Container::new(layout))),
        }
    }

    pub fn view(view: View) -> Self {
        Self {
            parent: ViewId::default(),
            content: Content::View(Box::new(view)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Horizontal,
    Vertical,
    // could explore stacked/tabbed
}

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug)]
pub struct Container {
    layout: Layout,
    children: Vec<ViewId>,
    /// Each child's share of the container, in the order of `children`: a drag of the
    /// separator between two of them moves weight from one to the other. Equal weights split
    /// the container evenly, as helix always has.
    weights: Vec<u32>,
    area: Rect,
}

/// The weight of a child in a container nobody has resized.
const DEFAULT_WEIGHT: u32 = 1 << 16;

/// The least a view keeps when a separator is dragged over it: its gutter and a few
/// columns of text across, a couple of lines and its statusline down.
const MIN_VIEW_WIDTH: u16 = 12;
const MIN_VIEW_HEIGHT: u16 = 3;

impl Container {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            children: Vec::new(),
            weights: Vec::new(),
            area: Rect::default(),
        }
    }

    /// Puts `child` at `pos` with the weight of the sibling it is split from, or the default
    /// in an empty container: a container never resized stays evenly split, and one resized
    /// keeps the proportions of the others.
    fn insert_child(&mut self, pos: usize, child: ViewId, sibling: Option<usize>) {
        let weight = sibling.map_or(DEFAULT_WEIGHT, |sibling| self.weights[sibling]);
        self.children.insert(pos, child);
        self.weights.insert(pos, weight);
    }

    /// Takes the child at `pos` out; the others share its room in proportion.
    fn remove_child(&mut self, pos: usize) {
        self.children.remove(pos);
        self.weights.remove(pos);
    }
}

/// The extent a container shares out among its children: its height when they are stacked,
/// its width less the separators' columns when they sit side by side.
fn shared_extent(container: &Container) -> u16 {
    match container.layout {
        Layout::Horizontal => container.area.height,
        Layout::Vertical => {
            let gaps = (container.children.len() as u16).saturating_sub(2);
            container.area.width.saturating_sub(gaps)
        }
    }
}

fn total_weight(weights: &[u32]) -> u64 {
    weights.iter().map(|&weight| u64::from(weight)).sum()
}

/// A child's part of `extent`: with equal weights, exactly the even split helix has always
/// made, the rounding left to the last child.
fn share(extent: u16, weight: u32, total: u64) -> u16 {
    (u64::from(extent) * u64::from(weight) / total.max(1)) as u16
}

/// A boundary between two neighbouring children of a container, which a drag moves: the
/// column between two views side by side, or the statusline of a view with another below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Separator {
    container: ViewId,
    /// It sits after this child, before the next one.
    index: usize,
}

impl Default for Container {
    fn default() -> Self {
        Self::new(Layout::Vertical)
    }
}

impl Tree {
    pub fn new(area: Rect) -> Self {
        let root = Node::container(Layout::Vertical);

        let mut nodes = SlotMap::with_key();
        let root = nodes.insert(root);

        // root is it's own parent
        nodes[root].parent = root;

        Self {
            root,
            focus: root,
            // fullscreen: false,
            area,
            nodes,
            stack: Vec::new(),
        }
    }

    pub fn insert(&mut self, view: View) -> ViewId {
        let focus = self.focus;
        let parent = self.nodes[focus].parent;
        let mut node = Node::view(view);
        node.parent = parent;
        let node = self.nodes.insert(node);
        self.get_mut(node).id = node;

        let container = match &mut self.nodes[parent] {
            Node {
                content: Content::Container(container),
                ..
            } => container,
            _ => unreachable!(),
        };

        // insert node after the current item if there is children already
        let sibling = if container.children.is_empty() {
            None
        } else {
            let pos = container
                .children
                .iter()
                .position(|&child| child == focus)
                .unwrap();
            Some(pos)
        };
        let pos = sibling.map_or(0, |sibling| sibling + 1);

        container.insert_child(pos, node, sibling);
        // focus the new node
        self.focus = node;

        // recalculate all the sizes
        self.recalculate();

        node
    }

    pub fn split(&mut self, view: View, layout: Layout) -> ViewId {
        let focus = self.focus;
        let parent = self.nodes[focus].parent;

        let node = Node::view(view);
        let node = self.nodes.insert(node);
        self.get_mut(node).id = node;

        let container = match &mut self.nodes[parent] {
            Node {
                content: Content::Container(container),
                ..
            } => container,
            _ => unreachable!(),
        };
        if container.layout == layout {
            // insert node after the current item if there is children already
            let sibling = if container.children.is_empty() {
                None
            } else {
                let pos = container
                    .children
                    .iter()
                    .position(|&child| child == focus)
                    .unwrap();
                Some(pos)
            };
            let pos = sibling.map_or(0, |sibling| sibling + 1);
            container.insert_child(pos, node, sibling);
            self.nodes[node].parent = parent;
        } else {
            let mut split = Node::container(layout);
            split.parent = parent;
            let split = self.nodes.insert(split);

            let container = match &mut self.nodes[split] {
                Node {
                    content: Content::Container(container),
                    ..
                } => container,
                _ => unreachable!(),
            };
            container.insert_child(0, focus, None);
            container.insert_child(1, node, Some(0));
            self.nodes[focus].parent = split;
            self.nodes[node].parent = split;

            let container = match &mut self.nodes[parent] {
                Node {
                    content: Content::Container(container),
                    ..
                } => container,
                _ => unreachable!(),
            };

            let pos = container
                .children
                .iter()
                .position(|&child| child == focus)
                .unwrap();

            // replace focus on parent with split
            container.children[pos] = split;
        }

        // focus the new node
        self.focus = node;

        // recalculate all the sizes
        self.recalculate();

        node
    }

    /// Get a mutable reference to a [Container] by index.
    /// # Panics
    /// Panics if `index` is not in self.nodes, or if the node's content is not a [Content::Container].
    fn container_mut(&mut self, index: ViewId) -> &mut Container {
        match &mut self.nodes[index] {
            Node {
                content: Content::Container(container),
                ..
            } => container,
            _ => unreachable!(),
        }
    }

    fn remove_or_replace(&mut self, child: ViewId, replacement: Option<ViewId>) {
        let parent = self.nodes[child].parent;

        self.nodes.remove(child);

        let container = self.container_mut(parent);
        let pos = container
            .children
            .iter()
            .position(|&item| item == child)
            .unwrap();

        if let Some(new) = replacement {
            container.children[pos] = new;
            self.nodes[new].parent = parent;
        } else {
            container.remove_child(pos);
        }
    }

    pub fn remove(&mut self, index: ViewId) {
        if self.focus == index {
            // focus on something else
            self.focus = self.prev();
        }

        let parent = self.nodes[index].parent;
        let parent_is_root = parent == self.root;

        self.remove_or_replace(index, None);

        let parent_container = self.container_mut(parent);
        if parent_container.children.len() == 1 && !parent_is_root {
            // Lets merge the only child back to its grandparent so that Views
            // are equally spaced.
            let sibling = parent_container.children[0];
            parent_container.remove_child(0);
            self.remove_or_replace(parent, Some(sibling));
        }

        self.recalculate()
    }

    pub fn views(&self) -> impl Iterator<Item = (&View, bool)> {
        let focus = self.focus;
        self.nodes.iter().filter_map(move |(key, node)| match node {
            Node {
                content: Content::View(view),
                ..
            } => Some((view.as_ref(), focus == key)),
            _ => None,
        })
    }

    pub fn views_mut(&mut self) -> impl Iterator<Item = (&mut View, bool)> {
        let focus = self.focus;
        self.nodes
            .iter_mut()
            .filter_map(move |(key, node)| match node {
                Node {
                    content: Content::View(view),
                    ..
                } => Some((view.as_mut(), focus == key)),
                _ => None,
            })
    }

    /// Get reference to a [View] by index.
    /// # Panics
    ///
    /// Panics if `index` is not in self.nodes, or if the node's content is not [Content::View]. This can be checked with [Self::contains].
    pub fn get(&self, index: ViewId) -> &View {
        self.try_get(index).unwrap()
    }

    /// Try to get reference to a [View] by index. Returns `None` if node content is not a [`Content::View`].
    ///
    /// Does not panic if the view does not exists anymore.
    pub fn try_get(&self, index: ViewId) -> Option<&View> {
        match self.nodes.get(index) {
            Some(Node {
                content: Content::View(view),
                ..
            }) => Some(view),
            _ => None,
        }
    }

    /// Get a mutable reference to a [View] by index.
    /// # Panics
    ///
    /// Panics if `index` is not in self.nodes, or if the node's content is not [Content::View]. This can be checked with [Self::contains].
    pub fn get_mut(&mut self, index: ViewId) -> &mut View {
        match &mut self.nodes[index] {
            Node {
                content: Content::View(view),
                ..
            } => view,
            _ => unreachable!(),
        }
    }

    /// Check if tree contains a [Node] with a given index.
    pub fn contains(&self, index: ViewId) -> bool {
        self.nodes.contains_key(index)
    }

    pub fn is_empty(&self) -> bool {
        match &self.nodes[self.root] {
            Node {
                content: Content::Container(container),
                ..
            } => container.children.is_empty(),
            _ => unreachable!(),
        }
    }

    pub fn resize(&mut self, area: Rect) -> bool {
        if self.area != area {
            self.area = area;
            self.recalculate();
            return true;
        }
        false
    }

    pub fn recalculate(&mut self) {
        if self.is_empty() {
            // There are no more views, so the tree should focus itself again.
            self.focus = self.root;

            return;
        }

        self.stack.push((self.root, self.area));

        // take the area
        // fetch the node
        // a) node is view, give it whole area
        // b) node is container, calculate areas for each child and push them on the stack

        while let Some((key, area)) = self.stack.pop() {
            let node = &mut self.nodes[key];

            match &mut node.content {
                Content::View(view) => {
                    // debug!!("setting view area {:?}", area);
                    view.area = area;
                } // TODO: call f()
                Content::Container(container) => {
                    // debug!!("setting container area {:?}", area);
                    container.area = area;

                    match container.layout {
                        Layout::Horizontal => {
                            let len = container.children.len();
                            let total = total_weight(&container.weights);

                            let mut child_y = area.y;

                            for (i, child) in container.children.iter().enumerate() {
                                let height = share(area.height, container.weights[i], total);
                                let mut area = Rect::new(
                                    container.area.x,
                                    child_y,
                                    container.area.width,
                                    height,
                                );
                                child_y += height;

                                // last child takes the remaining width because we can get uneven
                                // space from rounding
                                if i == len - 1 {
                                    area.height = container.area.y + container.area.height - area.y;
                                }

                                self.stack.push((*child, area));
                            }
                        }
                        Layout::Vertical => {
                            let len = container.children.len();

                            let inner_gap = 1u16;
                            let used_area = shared_extent(container);
                            let total = total_weight(&container.weights);

                            let mut child_x = area.x;

                            for (i, child) in container.children.iter().enumerate() {
                                let width = share(used_area, container.weights[i], total);
                                let mut area = Rect::new(
                                    child_x,
                                    container.area.y,
                                    width,
                                    container.area.height,
                                );
                                child_x += width + inner_gap;

                                // last child takes the remaining width because we can get uneven
                                // space from rounding
                                if i == len - 1 {
                                    area.width = container.area.x + container.area.width - area.x;
                                }

                                self.stack.push((*child, area));
                            }
                        }
                    }
                }
            }
        }
    }

    /// The separator at a screen cell, if the cell is one: the column between two views side
    /// by side, or the statusline of a view that has another below it.
    pub fn separator_at(&self, row: u16, column: u16) -> Option<Separator> {
        self.nodes.iter().find_map(|(key, node)| {
            let Content::Container(container) = &node.content else {
                return None;
            };
            let last = container.children.len().checked_sub(1)?;
            (0..last).find_map(|index| {
                let child = self.node_area(container.children[index]);
                let hit = match container.layout {
                    Layout::Vertical => {
                        column == child.right()
                            && row >= container.area.top()
                            && row < container.area.bottom()
                    }
                    Layout::Horizontal => {
                        row + 1 == child.bottom()
                            && column >= child.left()
                            && column < child.right()
                    }
                };
                hit.then_some(Separator {
                    container: key,
                    index,
                })
            })
        })
    }

    /// Moves a separator to the given cell, as far as leaves each side its smallest size.
    /// Returns false when the separator is gone, the layout having changed under the drag.
    pub fn drag_separator(&mut self, separator: Separator, row: u16, column: u16) -> bool {
        let Some(Node {
            content: Content::Container(container),
            ..
        }) = self.nodes.get(separator.container)
        else {
            return false;
        };
        if separator.index + 1 >= container.children.len() {
            return false;
        }
        let layout = container.layout;
        let extent = shared_extent(container);
        let first = container.children[separator.index];
        let second = container.children[separator.index + 1];
        let before = self.node_area(first);
        let after = self.node_area(second);

        // How much room the two share, how much of it the first would take, and the least
        // each needs, all along the container's own axis.
        let (room, wanted, first_min, second_min) = match layout {
            Layout::Vertical => (
                after.right().saturating_sub(before.left()),
                column.saturating_sub(before.left()),
                self.min_extent(first, layout),
                self.min_extent(second, layout),
            ),
            Layout::Horizontal => (
                after.bottom().saturating_sub(before.top()),
                (row + 1).saturating_sub(before.top()),
                self.min_extent(first, layout),
                self.min_extent(second, layout),
            ),
        };
        // Side by side, the separator's own column sits between the two.
        let gap = match layout {
            Layout::Vertical => 1,
            Layout::Horizontal => 0,
        };
        let most = room.saturating_sub(gap + second_min).max(first_min);
        let taken = wanted.clamp(first_min, most);

        // The weight whose share of the container, rounded down as `recalculate` rounds it,
        // is exactly `taken`: the separator lands under the mouse, and the pair keeps its
        // sum, so the children around it keep theirs.
        let container = self.container_mut(separator.container);
        let pair = container.weights[separator.index] + container.weights[separator.index + 1];
        let total = total_weight(&container.weights);
        let first_weight = (u64::from(taken) * total).div_ceil(u64::from(extent.max(1)));
        let first_weight = first_weight.clamp(1, u64::from(pair - 1)) as u32;
        container.weights[separator.index] = first_weight;
        container.weights[separator.index + 1] = pair - first_weight;
        self.recalculate();
        true
    }

    fn node_area(&self, id: ViewId) -> Rect {
        match &self.nodes[id].content {
            Content::View(view) => view.area,
            Content::Container(container) => container.area,
        }
    }

    /// The least a node can be along `axis`: a view's minimum, the sum of its children's
    /// for a container laid out along that axis, the largest of them for one across it.
    fn min_extent(&self, id: ViewId, axis: Layout) -> u16 {
        match &self.nodes[id].content {
            Content::View(_) => match axis {
                Layout::Vertical => MIN_VIEW_WIDTH,
                Layout::Horizontal => MIN_VIEW_HEIGHT,
            },
            Content::Container(container) => {
                let children = container
                    .children
                    .iter()
                    .map(|&child| self.min_extent(child, axis));
                if container.layout == axis {
                    // Side by side, every view but the last is followed by its separator.
                    let gaps = match axis {
                        Layout::Vertical => container.children.len().saturating_sub(1) as u16,
                        Layout::Horizontal => 0,
                    };
                    children.sum::<u16>() + gaps
                } else {
                    children.max().unwrap_or(0)
                }
            }
        }
    }

    pub fn traverse(&self) -> Traverse<'_> {
        Traverse::new(self)
    }

    // Finds the split in the given direction if it exists
    pub fn find_split_in_direction(&self, id: ViewId, direction: Direction) -> Option<ViewId> {
        let parent = self.nodes[id].parent;
        // Base case, we found the root of the tree
        if parent == id {
            return None;
        }
        // Parent must always be a container
        let parent_container = match &self.nodes[parent].content {
            Content::Container(container) => container,
            Content::View(_) => unreachable!(),
        };

        match (direction, parent_container.layout) {
            (Direction::Up, Layout::Vertical)
            | (Direction::Left, Layout::Horizontal)
            | (Direction::Right, Layout::Horizontal)
            | (Direction::Down, Layout::Vertical) => {
                // The desired direction of movement is not possible within
                // the parent container so the search must continue closer to
                // the root of the split tree.
                self.find_split_in_direction(parent, direction)
            }
            (Direction::Up, Layout::Horizontal)
            | (Direction::Down, Layout::Horizontal)
            | (Direction::Left, Layout::Vertical)
            | (Direction::Right, Layout::Vertical) => {
                // It's possible to move in the desired direction within
                // the parent container so an attempt is made to find the
                // correct child.
                match self.find_child(id, &parent_container.children, direction) {
                    // Child is found, search is ended
                    Some(id) => Some(id),
                    // A child is not found. This could be because of either two scenarios
                    // 1. Its not possible to move in the desired direction, and search should end
                    // 2. A layout like the following with focus at X and desired direction Right
                    // | _ | x |   |
                    // | _ _ _ |   |
                    // | _ _ _ |   |
                    // The container containing X ends at X so no rightward movement is possible
                    // however there still exists another view/container to the right that hasn't
                    // been explored. Thus another search is done here in the parent container
                    // before concluding it's not possible to move in the desired direction.
                    None => self.find_split_in_direction(parent, direction),
                }
            }
        }
    }

    fn find_child(&self, id: ViewId, children: &[ViewId], direction: Direction) -> Option<ViewId> {
        let mut child_id = match direction {
            // index wise in the child list the Up and Left represents a -1
            // thus reversed iterator.
            Direction::Up | Direction::Left => children
                .iter()
                .rev()
                .skip_while(|i| **i != id)
                .copied()
                .nth(1)?,
            // Down and Right => +1 index wise in the child list
            Direction::Down | Direction::Right => {
                children.iter().skip_while(|i| **i != id).copied().nth(1)?
            }
        };
        let (current_x, current_y) = match &self.nodes[self.focus].content {
            Content::View(current_view) => (current_view.area.left(), current_view.area.top()),
            Content::Container(_) => unreachable!(),
        };

        // If the child is a container the search finds the closest container child
        // visually based on screen location.
        while let Content::Container(container) = &self.nodes[child_id].content {
            match (direction, container.layout) {
                (_, Layout::Vertical) => {
                    // find closest split based on x because y is irrelevant
                    // in a vertical container (and already correct based on previous search)
                    child_id = *container.children.iter().min_by_key(|id| {
                        let x = match &self.nodes[**id].content {
                            Content::View(view) => view.area.left(),
                            Content::Container(container) => container.area.left(),
                        };
                        (current_x as i16 - x as i16).abs()
                    })?;
                }
                (_, Layout::Horizontal) => {
                    // find closest split based on y because x is irrelevant
                    // in a horizontal container (and already correct based on previous search)
                    child_id = *container.children.iter().min_by_key(|id| {
                        let y = match &self.nodes[**id].content {
                            Content::View(view) => view.area.top(),
                            Content::Container(container) => container.area.top(),
                        };
                        (current_y as i16 - y as i16).abs()
                    })?;
                }
            }
        }
        Some(child_id)
    }

    pub fn prev(&self) -> ViewId {
        // This function is very dumb, but that's because we don't store any parent links.
        // (we'd be able to go parent.prev_sibling() recursively until we find something)
        // For now that's okay though, since it's unlikely you'll be able to open a large enough
        // number of splits to notice.

        let mut views = self
            .traverse()
            .rev()
            .skip_while(|&(id, _view)| id != self.focus)
            .skip(1); // Skip focused value
        if let Some((id, _)) = views.next() {
            id
        } else {
            // extremely crude, take the last item
            let (key, _) = self.traverse().next_back().unwrap();
            key
        }
    }

    pub fn next(&self) -> ViewId {
        // This function is very dumb, but that's because we don't store any parent links.
        // (we'd be able to go parent.next_sibling() recursively until we find something)
        // For now that's okay though, since it's unlikely you'll be able to open a large enough
        // number of splits to notice.

        let mut views = self
            .traverse()
            .skip_while(|&(id, _view)| id != self.focus)
            .skip(1); // Skip focused value
        if let Some((id, _)) = views.next() {
            id
        } else {
            // extremely crude, take the first item again
            let (key, _) = self.traverse().next().unwrap();
            key
        }
    }

    pub fn transpose(&mut self) {
        let focus = self.focus;
        let parent = self.nodes[focus].parent;
        if let Content::Container(container) = &mut self.nodes[parent].content {
            container.layout = match container.layout {
                Layout::Vertical => Layout::Horizontal,
                Layout::Horizontal => Layout::Vertical,
            };
            self.recalculate();
        }
    }

    pub fn swap_split_in_direction(&mut self, direction: Direction) -> Option<()> {
        let focus = self.focus;
        let target = self.find_split_in_direction(focus, direction)?;
        let focus_parent = self.nodes[focus].parent;
        let target_parent = self.nodes[target].parent;

        if focus_parent == target_parent {
            let parent = focus_parent;
            let [parent, focus, target] = self.nodes.get_disjoint_mut([parent, focus, target])?;
            match (&mut parent.content, &mut focus.content, &mut target.content) {
                (
                    Content::Container(parent),
                    Content::View(focus_view),
                    Content::View(target_view),
                ) => {
                    let focus_pos = parent.children.iter().position(|id| focus_view.id == *id)?;
                    let target_pos = parent
                        .children
                        .iter()
                        .position(|id| target_view.id == *id)?;
                    // swap node positions so that traversal order is kept
                    parent.children[focus_pos] = target_view.id;
                    parent.children[target_pos] = focus_view.id;
                    // swap area so that views rendered at the correct location
                    std::mem::swap(&mut focus_view.area, &mut target_view.area);

                    Some(())
                }
                _ => unreachable!(),
            }
        } else {
            let [focus_parent, target_parent, focus, target] =
                self.nodes
                    .get_disjoint_mut([focus_parent, target_parent, focus, target])?;
            match (
                &mut focus_parent.content,
                &mut target_parent.content,
                &mut focus.content,
                &mut target.content,
            ) {
                (
                    Content::Container(focus_parent),
                    Content::Container(target_parent),
                    Content::View(focus_view),
                    Content::View(target_view),
                ) => {
                    let focus_pos = focus_parent
                        .children
                        .iter()
                        .position(|id| focus_view.id == *id)?;
                    let target_pos = target_parent
                        .children
                        .iter()
                        .position(|id| target_view.id == *id)?;
                    // re-parent target and focus nodes
                    std::mem::swap(
                        &mut focus_parent.children[focus_pos],
                        &mut target_parent.children[target_pos],
                    );
                    std::mem::swap(&mut focus.parent, &mut target.parent);
                    // swap area so that views rendered at the correct location
                    std::mem::swap(&mut focus_view.area, &mut target_view.area);

                    Some(())
                }
                _ => unreachable!(),
            }
        }
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    /// The splits down to the views, in the order they sit in: what a session writes down.
    pub fn panes(&self) -> Pane {
        self.pane_of(self.root)
    }

    fn pane_of(&self, id: ViewId) -> Pane {
        match &self.nodes[id].content {
            Content::View(_) => Pane::View(id),
            Content::Container(container) => {
                let panes = container
                    .children
                    .iter()
                    .map(|child| self.pane_of(*child))
                    .collect();
                Pane::Split {
                    layout: container.layout,
                    weights: container.weights.clone(),
                    panes,
                }
            }
        }
    }

    /// Gives the container holding exactly `views`, in that order, the shares `weights`,
    /// one per view: how a session puts a resized split back. Nothing changes, and false
    /// comes back, when the views are not one container's children or the counts differ.
    pub fn set_weights(&mut self, views: &[ViewId], weights: &[u32]) -> bool {
        let Some(first) = views.first() else {
            return false;
        };
        if weights.len() != views.len() || weights.contains(&0) {
            return false;
        }
        let parent = self.nodes[*first].parent;
        let Some(Node {
            content: Content::Container(container),
            ..
        }) = self.nodes.get_mut(parent)
        else {
            return false;
        };
        if container.children != views {
            return false;
        }
        container.weights = weights.to_vec();
        self.recalculate();
        true
    }
}

/// A view, or a container's layout and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pane {
    View(ViewId),
    Split {
        layout: Layout,
        /// Each pane's share of the split, in the order of `panes`.
        weights: Vec<u32>,
        panes: Vec<Pane>,
    },
}

#[derive(Debug)]
pub struct Traverse<'a> {
    tree: &'a Tree,
    stack: Vec<ViewId>, // TODO: reuse the one we use on update
}

impl<'a> Traverse<'a> {
    fn new(tree: &'a Tree) -> Self {
        Self {
            tree,
            stack: vec![tree.root],
        }
    }
}

impl<'a> Iterator for Traverse<'a> {
    type Item = (ViewId, &'a View);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let key = self.stack.pop()?;

            let node = &self.tree.nodes[key];

            match &node.content {
                Content::View(view) => return Some((key, view)),
                Content::Container(container) => {
                    self.stack.extend(container.children.iter().rev());
                }
            }
        }
    }
}

impl DoubleEndedIterator for Traverse<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        loop {
            let key = self.stack.pop()?;

            let node = &self.tree.nodes[key];

            match &node.content {
                Content::View(view) => return Some((key, view)),
                Content::Container(container) => {
                    self.stack.extend(container.children.iter());
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::editor::GutterConfig;
    use crate::DocumentId;

    #[test]
    fn find_split_in_direction() {
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: 180,
            height: 80,
        });
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, 180, 80);
        tree.insert(view);

        let l0 = tree.focus;
        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);
        let r0 = tree.focus;

        tree.focus = l0;
        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Horizontal);
        let l1 = tree.focus;

        tree.focus = l0;
        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);

        // Tree in test
        // | L0  | L2 |    |
        // |    L1    | R0 |
        let l2 = tree.focus;
        assert_eq!(Some(l0), tree.find_split_in_direction(l2, Direction::Left));
        assert_eq!(Some(l1), tree.find_split_in_direction(l2, Direction::Down));
        assert_eq!(Some(r0), tree.find_split_in_direction(l2, Direction::Right));
        assert_eq!(None, tree.find_split_in_direction(l2, Direction::Up));

        tree.focus = l1;
        assert_eq!(None, tree.find_split_in_direction(l1, Direction::Left));
        assert_eq!(None, tree.find_split_in_direction(l1, Direction::Down));
        assert_eq!(Some(r0), tree.find_split_in_direction(l1, Direction::Right));
        assert_eq!(Some(l0), tree.find_split_in_direction(l1, Direction::Up));

        tree.focus = l0;
        assert_eq!(None, tree.find_split_in_direction(l0, Direction::Left));
        assert_eq!(Some(l1), tree.find_split_in_direction(l0, Direction::Down));
        assert_eq!(Some(l2), tree.find_split_in_direction(l0, Direction::Right));
        assert_eq!(None, tree.find_split_in_direction(l0, Direction::Up));

        tree.focus = r0;
        assert_eq!(Some(l2), tree.find_split_in_direction(r0, Direction::Left));
        assert_eq!(None, tree.find_split_in_direction(r0, Direction::Down));
        assert_eq!(None, tree.find_split_in_direction(r0, Direction::Right));
        assert_eq!(None, tree.find_split_in_direction(r0, Direction::Up));
    }

    #[test]
    fn swap_split_in_direction() {
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: 180,
            height: 80,
        });

        let doc_l0 = DocumentId::default();
        let mut view = View::new(doc_l0, GutterConfig::default());
        view.area = Rect::new(0, 0, 180, 80);
        tree.insert(view);

        let l0 = tree.focus;

        let doc_r0 = DocumentId::default();
        let view = View::new(doc_r0, GutterConfig::default());
        tree.split(view, Layout::Vertical);
        let r0 = tree.focus;

        tree.focus = l0;

        let doc_l1 = DocumentId::default();
        let view = View::new(doc_l1, GutterConfig::default());
        tree.split(view, Layout::Horizontal);
        let l1 = tree.focus;

        tree.focus = l0;

        let doc_l2 = DocumentId::default();
        let view = View::new(doc_l2, GutterConfig::default());
        tree.split(view, Layout::Vertical);
        let l2 = tree.focus;

        // Views in test
        // | L0  | L2 |    |
        // |    L1    | R0 |

        // Document IDs in test
        // | l0  | l2 |    |
        // |    l1    | r0 |

        fn doc_id(tree: &Tree, view_id: ViewId) -> Option<DocumentId> {
            if let Content::View(view) = &tree.nodes[view_id].content {
                Some(view.doc)
            } else {
                None
            }
        }

        tree.focus = l0;
        // `*` marks the view in focus from view table (here L0)
        // | l0*  | l2 |    |
        // |    l1     | r0 |
        tree.swap_split_in_direction(Direction::Down);
        // | l1   | l2 |    |
        // |    l0*    | r0 |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_l0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_r0));

        tree.swap_split_in_direction(Direction::Right);

        // | l1  | l2 |     |
        // |    r0    | l0* |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_r0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        // cannot swap, nothing changes
        tree.swap_split_in_direction(Direction::Up);
        // | l1  | l2 |     |
        // |    r0    | l0* |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_r0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        // cannot swap, nothing changes
        tree.swap_split_in_direction(Direction::Down);
        // | l1  | l2 |     |
        // |    r0    | l0* |
        assert_eq!(tree.focus, l0);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_r0));
        assert_eq!(doc_id(&tree, l2), Some(doc_l2));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        tree.focus = l2;
        // | l1  | l2* |    |
        // |    r0     | l0 |

        tree.swap_split_in_direction(Direction::Down);
        // | l1  | r0  |    |
        // |    l2*    | l0 |
        assert_eq!(tree.focus, l2);
        assert_eq!(doc_id(&tree, l0), Some(doc_l1));
        assert_eq!(doc_id(&tree, l1), Some(doc_l2));
        assert_eq!(doc_id(&tree, l2), Some(doc_r0));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));

        tree.swap_split_in_direction(Direction::Up);
        // | l2* | r0 |    |
        // |    l1    | l0 |
        assert_eq!(tree.focus, l2);
        assert_eq!(doc_id(&tree, l0), Some(doc_l2));
        assert_eq!(doc_id(&tree, l1), Some(doc_l1));
        assert_eq!(doc_id(&tree, l2), Some(doc_r0));
        assert_eq!(doc_id(&tree, r0), Some(doc_l0));
    }

    #[test]
    fn all_vertical_views_have_same_width() {
        let tree_area_width = 180;
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: tree_area_width,
            height: 80,
        });
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, 180, 80);
        tree.insert(view);

        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);

        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Horizontal);

        tree.remove(tree.focus);

        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, Layout::Vertical);

        // Make sure that we only have one level in the tree.
        assert_eq!(3, tree.views().count());
        assert_eq!(
            vec![
                tree_area_width / 3 - 1, // gap here
                tree_area_width / 3 - 1, // gap here
                tree_area_width / 3
            ],
            tree.views()
                .map(|(view, _)| view.area.width)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn vsplit_gap_rounding() {
        let (tree_area_width, tree_area_height) = (80, 24);
        let mut tree = Tree::new(Rect {
            x: 0,
            y: 0,
            width: tree_area_width,
            height: tree_area_height,
        });
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, tree_area_width, tree_area_height);
        tree.insert(view);

        for _ in 0..9 {
            let view = View::new(DocumentId::default(), GutterConfig::default());
            tree.split(view, Layout::Vertical);
        }

        assert_eq!(10, tree.views().count());
        assert_eq!(
            std::iter::repeat_n(7, 9)
                .chain(Some(8)) // Rounding in `recalculate`.
                .collect::<Vec<_>>(),
            tree.views()
                .map(|(view, _)| view.area.width)
                .collect::<Vec<_>>()
        );
    }

    fn tree_with_views(width: u16, height: u16) -> Tree {
        let mut tree = Tree::new(Rect::new(0, 0, width, height));
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(0, 0, width, height);
        tree.insert(view);
        tree
    }

    fn split_view(tree: &mut Tree, layout: Layout) -> ViewId {
        let view = View::new(DocumentId::default(), GutterConfig::default());
        tree.split(view, layout)
    }

    fn widths(tree: &Tree) -> Vec<u16> {
        tree.traverse()
            .map(|(id, _)| tree.get(id).area.width)
            .collect()
    }

    fn heights(tree: &Tree) -> Vec<u16> {
        tree.traverse()
            .map(|(id, _)| tree.get(id).area.height)
            .collect()
    }

    #[test]
    fn dragging_a_vertical_separator_moves_it_to_the_mouse() {
        let mut tree = tree_with_views(80, 24);
        split_view(&mut tree, Layout::Vertical);
        // The separator's column comes out of the last view.
        assert_eq!(widths(&tree), vec![40, 39]);

        let separator = tree
            .separator_at(5, 40)
            .expect("the column between the views");
        assert_eq!(tree.separator_at(5, 39), None);
        assert_eq!(tree.separator_at(5, 41), None);

        assert!(tree.drag_separator(separator, 5, 25));
        assert_eq!(widths(&tree), vec![25, 54]);
        assert_eq!(tree.separator_at(5, 25), Some(separator));
    }

    #[test]
    fn dragging_a_statusline_resizes_the_views_above_and_below() {
        let mut tree = tree_with_views(80, 24);
        split_view(&mut tree, Layout::Horizontal);
        assert_eq!(heights(&tree), vec![12, 12]);

        // The upper view's statusline is its last row.
        let separator = tree.separator_at(11, 30).expect("the upper statusline");
        assert_eq!(tree.separator_at(10, 30), None);

        assert!(tree.drag_separator(separator, 7, 30));
        assert_eq!(heights(&tree), vec![8, 16]);
    }

    #[test]
    fn a_drag_leaves_each_side_its_smallest_size() {
        let mut tree = tree_with_views(80, 24);
        split_view(&mut tree, Layout::Vertical);
        let separator = tree.separator_at(0, 40).unwrap();

        tree.drag_separator(separator, 0, 0);
        assert_eq!(widths(&tree)[0], MIN_VIEW_WIDTH);

        tree.drag_separator(separator, 0, 79);
        assert_eq!(widths(&tree)[1], MIN_VIEW_WIDTH);
    }

    #[test]
    fn a_split_after_a_resize_weighs_the_new_view_as_the_one_it_splits() {
        let mut tree = tree_with_views(100, 24);
        let right = split_view(&mut tree, Layout::Vertical);
        let separator = tree.separator_at(0, 50).unwrap();
        tree.drag_separator(separator, 0, 75);
        assert_eq!(widths(&tree), vec![75, 24]);

        // Three to one, and the new view weighs as the right one: three to one to one.
        tree.focus = right;
        split_view(&mut tree, Layout::Vertical);
        assert_eq!(widths(&tree), vec![59, 19, 20]);
    }

    #[test]
    fn the_panes_say_how_the_views_are_split() {
        let mut tree = tree_with_views(90, 24);
        let first = tree.focus;
        let right = split_view(&mut tree, Layout::Vertical);
        let below = split_view(&mut tree, Layout::Horizontal);

        let panes = tree.panes();

        assert_eq!(
            panes,
            Pane::Split {
                layout: Layout::Vertical,
                weights: vec![DEFAULT_WEIGHT, DEFAULT_WEIGHT],
                panes: vec![
                    Pane::View(first),
                    Pane::Split {
                        layout: Layout::Horizontal,
                        weights: vec![DEFAULT_WEIGHT, DEFAULT_WEIGHT],
                        panes: vec![Pane::View(right), Pane::View(below)],
                    },
                ],
            }
        );

        // The shares a session wrote down go back on, in the panes' order: the two
        // stacked views three to one, over 24 rows.
        assert!(tree.set_weights(&[right, below], &[3, 1]));
        assert_eq!(heights(&tree), vec![24, 18, 6]);
        // Not for views that are not one container's children, not for a count that
        // is not theirs, and not for a share of nothing.
        assert!(!tree.set_weights(&[first, right], &[1, 1]));
        assert!(!tree.set_weights(&[right, below], &[1]));
        assert!(!tree.set_weights(&[right, below], &[0, 1]));
        assert_eq!(heights(&tree), vec![24, 18, 6]);
    }

    #[test]
    fn closing_a_view_gives_its_room_to_the_others() {
        let mut tree = tree_with_views(90, 24);
        let middle = split_view(&mut tree, Layout::Vertical);
        split_view(&mut tree, Layout::Vertical);
        let separator = tree.separator_at(0, widths(&tree)[0]).unwrap();
        tree.drag_separator(separator, 0, 20);

        tree.remove(middle);
        assert_eq!(tree.views().count(), 2);
        assert_eq!(widths(&tree).iter().sum::<u16>() + 1, 90);
    }

    #[test]
    fn swapping_views_keeps_the_sizes_where_they_were() {
        let mut tree = tree_with_views(80, 24);
        let right = split_view(&mut tree, Layout::Vertical);
        let separator = tree.separator_at(0, 40).unwrap();
        tree.drag_separator(separator, 0, 60);
        assert_eq!(widths(&tree), vec![60, 19]);

        tree.focus = right;
        tree.swap_split_in_direction(Direction::Left).unwrap();
        tree.recalculate();
        assert_eq!(widths(&tree), vec![60, 19]);
    }

    #[test]
    fn a_nested_split_moves_only_its_own_boundary() {
        let mut tree = tree_with_views(80, 24);
        split_view(&mut tree, Layout::Vertical);
        // The right view split in two, one above the other.
        split_view(&mut tree, Layout::Horizontal);
        assert_eq!(widths(&tree), vec![40, 39, 39]);

        // The statusline of the upper right view moves that boundary, and nothing else.
        let separator = tree
            .separator_at(11, 60)
            .expect("the upper right statusline");
        assert_eq!(tree.separator_at(11, 20), None);
        tree.drag_separator(separator, 5, 60);
        assert_eq!(heights(&tree), vec![24, 6, 18]);
        assert_eq!(widths(&tree), vec![40, 39, 39]);

        // The column between the halves moves both right views together, and no further
        // than leaves the right half its smallest.
        let separator = tree.separator_at(3, 40).unwrap();
        tree.drag_separator(separator, 3, 70);
        assert_eq!(widths(&tree), vec![67, 12, 12]);
    }
}
