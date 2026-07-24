//! Live drag-to-reorder — the "the list gets out of your way while you drag"
//! behaviour behind the tab strip's chips, the sidebar's rows and the sidebar's
//! group headers.
//!
//! Nothing floats above the window: the item you're dragging stays in the
//! list, dimmed, and travels by the list rearranging around it. gpui's drag
//! system is used only for what it's good at here — knowing a drag is live and
//! redrawing every mouse move — while its floating preview renders nothing.
//! What it doesn't offer at all is any way to know *where* the cursor is
//! mid-drag from inside a drop target: only a `drag_over` style and a final
//! `on_drop`, which is the "one tile swaps with another on release" model this
//! module replaces. Here the surface reads [`Window::mouse_position`] every
//! frame, asks [`Reorder`] where the dragged slot belongs *right now*, and
//! renders the list in that order, so what you see is already the result.
//!
//! The shape of it:
//!
//! | Step | Who | What |
//! |---|---|---|
//! | Measure | every slot, every frame | its own bounds into a per-frame cell |
//! | Freeze | `on_drag` | that cell becomes [`Reorder::rects`] for the whole drag |
//! | Preview | the surface, per frame | [`Reorder::target`] → [`Reorder::order`] |
//! | Track | the held slot | [`Reorder::held_offset`] → follows the cursor |
//! | Slide | each displaced slot | [`Reorder::flip_offset`] → animate to zero |
//! | Record | the surface, per frame | [`set_pending`] — the order a release would give |
//! | Commit | the root, on drag end | [`take_pending`] → `Tty7App::apply_tab_order` |
//!
//! **Geometry is frozen at drag start on purpose.** The preview reflow moves
//! the very slots the hit-testing reads, so measuring live would let the list
//! feed back into its own input and oscillate under a still cursor. Freezing
//! also means a mid-drag scroll isn't tracked — a deliberate trade for a rail
//! whose rows are a few dozen pixels tall.

use gpui::{Axis, Bounds, Pixels, Point, px};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

/// The app-wide slot for the one drag that can be live at a time. Shared by
/// `Rc` because the `on_drag` that opens it only gets `&mut App`.
pub(crate) type ReorderState = Rc<RefCell<Option<Reorder>>>;

/// Everything a surface needs to draw one frame of a live reorder.
pub(crate) struct Preview {
    /// Slot indices in the order to render them.
    pub(crate) order: Vec<usize>,
    /// The slot the held item currently occupies — where a release right now
    /// would land it.
    pub(crate) target: usize,
    /// The slot being dragged. It stays in the list like any other — the drag
    /// is drawn by the list rearranging, not by a card floating over it — and
    /// only wears a "picked up" dimming so you can see which one you have.
    pub(crate) from: usize,
    /// Bumped whenever the preview order changes; part of each slot's
    /// animation id so a slide restarts rather than resuming.
    pub(crate) generation: usize,
    /// Per slot (indexed by its *frozen* index), the offset to start this
    /// frame at so it slides into place. Zero for everything the last change
    /// didn't touch, and unused for [`Self::from`] — the held item doesn't
    /// slide, it tracks.
    pub(crate) offsets: Vec<Pixels>,
    /// Where to draw the held item relative to the slot it's laid out in, so
    /// it follows the cursor continuously instead of hopping slot to slot.
    pub(crate) held: Pixels,
}

/// This frame's preview of `surface`, if that's where the live drag started
/// and its frozen geometry still describes a list of `len` slots.
pub(crate) fn preview(
    state: &ReorderState,
    surface: &Surface,
    len: usize,
    pointer: Point<Pixels>,
) -> Option<Preview> {
    let state = state.borrow();
    let r = state.as_ref().filter(|r| r.covers(surface, len))?;
    let target = r.target(pointer);
    let (generation, prev) = r.begin_frame(target);
    Some(Preview {
        order: r.order(target),
        target,
        from: r.from,
        generation,
        offsets: (0..len)
            .map(|slot| r.flip_offset(slot, prev, target))
            .collect(),
        held: r.held_offset(pointer, target),
    })
}

/// Record the tab order the current preview implies, so releasing the mouse
/// applies exactly what the user was looking at.
///
/// The surface computes this every frame while it draws the preview, rather
/// than a drop handler working it out on release, because a drop handler only
/// fires when the pointer is over *that element* at release — release a hair
/// above the rail (easy when dragging a row upward) and the move would be
/// silently lost, the list snapping back. The drag ending is the commit, and
/// where the cursor happens to be at that moment doesn't enter into it.
pub(crate) fn set_pending(state: &ReorderState, surface: &Surface, order: Vec<usize>) {
    if let Some(r) = state.borrow().as_ref().filter(|r| r.surface == *surface) {
        *r.pending.borrow_mut() = Some(order);
    }
}

/// Forget the order recorded by the previous frame, at the start of every
/// frame a drag is live — the surfaces re-record it as they draw.
///
/// Without this the recording is a high-water mark rather than a snapshot: drag
/// a row down and back to its own slot and the surface stops recording (the
/// move is a no-op), so a stale "moved" order would survive to be committed by
/// a release the user made after visibly putting the row back. Same for a frame
/// where the drag's frozen geometry no longer matches the list ([`Reorder::covers`]
/// — a tab closed, or a git probe moved one to another group): nothing is drawn
/// and so nothing may commit.
pub(crate) fn clear_pending(state: &ReorderState) {
    if let Some(r) = state.borrow().as_ref() {
        r.pending.borrow_mut().take();
    }
}

/// Take the recorded order out of a finished drag — see [`set_pending`].
pub(crate) fn take_pending(state: &ReorderState) -> Option<Vec<usize>> {
    state.borrow_mut().take()?.pending.into_inner()
}

/// Which of the app's reorderable lists a drag belongs to. The sidebar rail
/// holds two at once — the rows inside a group, and the group blocks
/// themselves — so a surface asking "is this drag mine?" needs more than
/// "am I the sidebar". Rows carry their group's repo root (`None` = Scratch)
/// because a row drag must never reflow a sibling group: a tab's group comes
/// from its cwd, not from where it sits in the list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Surface {
    /// The horizontal title-bar strip. Display order is plain tab order.
    Strip,
    /// The rows of one sidebar group.
    SidebarRows(Option<PathBuf>),
    /// The sidebar's group blocks (header + its rows), dragged by the header.
    SidebarGroups,
}

/// One live drag-reorder. Created when gpui starts a drag, read by the surface
/// on every frame until the drop, then dropped.
pub(crate) struct Reorder {
    /// The list this drag belongs to; a surface ignores state that isn't its own.
    pub(crate) surface: Surface,
    /// Index of the dragged slot in the frozen order.
    pub(crate) from: usize,
    /// Every slot's bounds in display order, as measured on the last frame
    /// before the drag started (see the module docs on why they're frozen).
    rects: Vec<Bounds<Pixels>>,
    /// The axis the list runs along — vertical for the rail, horizontal for the strip.
    axis: Axis,
    /// The list's gap between slots, so a displaced slot's shift matches what
    /// the layout will actually do.
    gap: Pixels,
    /// Where inside the dragged slot the pointer grabbed it, so the slot's
    /// position is derived from the cursor exactly as gpui's floating preview is.
    grab: Point<Pixels>,
    /// The target the previous frame drew, and a counter bumped whenever it
    /// changes. The slide-in animation keys its element id off the counter, so
    /// a slot that has just been displaced restarts its slide instead of
    /// resuming a finished one.
    prev: Cell<usize>,
    generation: Cell<usize>,
    /// The tab order releasing right now would produce, refreshed every frame
    /// by the surface drawing the preview. See [`set_pending`].
    pending: RefCell<Option<Vec<usize>>>,
}

impl Reorder {
    pub(crate) fn new(
        surface: Surface,
        from: usize,
        rects: Vec<Bounds<Pixels>>,
        axis: Axis,
        gap: Pixels,
        grab: Point<Pixels>,
    ) -> Self {
        Self {
            surface,
            from,
            rects,
            axis,
            gap,
            grab,
            prev: Cell::new(from),
            generation: Cell::new(0),
            pending: RefCell::new(None),
        }
    }

    /// True when this state belongs to `surface` and its frozen geometry still
    /// describes a list of `len` slots — a tab closing mid-drag, or the git
    /// probe moving a tab to another group, invalidates it rather than letting
    /// stale indices reorder the wrong thing.
    pub(crate) fn covers(&self, surface: &Surface, len: usize) -> bool {
        self.surface == *surface && self.rects.len() == len && self.from < len
    }

    /// The scalar component along the list's axis.
    fn along(&self, p: Point<Pixels>) -> Pixels {
        match self.axis {
            Axis::Vertical => p.y,
            Axis::Horizontal => p.x,
        }
    }

    /// A slot's extent along the list's axis.
    fn extent(&self, b: &Bounds<Pixels>) -> Pixels {
        match self.axis {
            Axis::Vertical => b.size.height,
            Axis::Horizontal => b.size.width,
        }
    }

    /// How far a slot moves when the dragged one passes it: the dragged slot's
    /// extent plus the gap it also takes with it.
    fn shift(&self) -> Pixels {
        self.extent(&self.rects[self.from]) + self.gap
    }

    /// Where the dragged slot belongs for a cursor at `pointer`: how many of
    /// the other slots would sit before it.
    ///
    /// A neighbour yields once the held item covers half of it — its *trailing*
    /// edge past that neighbour's centre going forward, its *leading* edge past
    /// it going back. Edges rather than the held item's own centre, because the
    /// two are only equivalent when everything is the same size: in the rail a
    /// three-row group block is twice a one-row block, and a centre-to-centre
    /// test would demand the tall block's middle reach the short one's middle —
    /// pointer travel that runs off the top of the list, which is exactly the
    /// "big group won't move up" case. Half-overlap asks the same of both
    /// directions and of any pair of sizes.
    ///
    /// The comparison is against the *frozen* centres, never the reflowed ones,
    /// so the reflow can't move the number it's being compared to and the
    /// crossing can't chase itself under a still cursor.
    pub(crate) fn target(&self, pointer: Point<Pixels>) -> usize {
        let leading = self.free_origin(pointer);
        let trailing = leading + self.extent(&self.rects[self.from]);
        self.rects
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.from)
            .filter(|(i, r)| {
                let centre = self.along(r.origin) + self.extent(r) / 2.;
                if *i < self.from {
                    // Still above the held item: it hasn't reached back this far.
                    leading >= centre
                } else {
                    trailing > centre
                }
            })
            .count()
    }

    /// Where the dragged slot's leading edge is, following the pointer without
    /// limit: the cursor less where inside the slot it was grabbed, so the item
    /// sits under the cursor exactly where you picked it up.
    fn free_origin(&self, pointer: Point<Pixels>) -> Pixels {
        self.along(pointer) - self.along(self.grab)
    }

    /// [`Self::free_origin`] confined to the list's own span, so dragging far
    /// past either end parks the item against that end instead of sending it
    /// off across the window. Only the *drawing* is clamped — [`Self::target`]
    /// reads the free position, so pushing past the last slot still selects it.
    fn held_origin(&self, pointer: Point<Pixels>) -> Pixels {
        let first = self.along(self.rects[0].origin);
        let last = self.rects.last().expect("non-empty");
        let end = self.along(last.origin) + self.extent(last);
        self.free_origin(pointer)
            .clamp(first, end - self.extent(&self.rects[self.from]))
    }

    /// The offset to draw the dragged slot at so it tracks the pointer: the
    /// distance from where the list has *laid it out* this frame (its slot
    /// under `target`) to where the cursor is actually holding it.
    ///
    /// This is what makes the drag feel attached rather than stepwise — the
    /// held item moves pixel-for-pixel with the mouse, and the reflow of the
    /// others is the only thing that snaps.
    pub(crate) fn held_offset(&self, pointer: Point<Pixels>, target: usize) -> Pixels {
        let home = self.along(self.rects[self.from].origin);
        self.held_origin(pointer) - (home + self.displacement(self.from, target))
    }

    /// Slot indices in preview order: the dragged one lifted out of `from` and
    /// dropped back in at `target`.
    pub(crate) fn order(&self, target: usize) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.rects.len()).collect();
        let dragged = order.remove(self.from);
        order.insert(target.min(order.len()), dragged);
        order
    }

    /// Open a frame previewing `target`: returns the animation generation to
    /// key slide-ins on, and the target the previous frame drew — which
    /// [`Self::flip_offset`] measures the slide from. Call once per frame,
    /// before laying the slots out.
    pub(crate) fn begin_frame(&self, target: usize) -> (usize, usize) {
        let prev = self.prev.get();
        if prev != target {
            self.generation.set(self.generation.get() + 1);
            self.prev.set(target);
        }
        (self.generation.get(), prev)
    }

    /// Where the slot frozen at index `slot` sits under a given preview,
    /// relative to its resting place.
    ///
    /// The displaced slots each close up by one dragged-slot pitch, in the
    /// direction the drag came from. The dragged slot itself moves the other
    /// way by everything it has jumped over — it stays in the list rather than
    /// floating above it, so it has a position to be displaced to like anyone
    /// else, and the two sides always add up to a swap.
    fn displacement(&self, slot: usize, target: usize) -> Pixels {
        if slot == self.from {
            // Sum the pitches of the slots crossed, since rows differ in height.
            let crossed = if target > self.from {
                self.from + 1..=target
            } else {
                target..=self.from.saturating_sub(1)
            };
            let span: Pixels = crossed
                .filter(|&i| i != self.from && i < self.rects.len())
                .map(|i| self.extent(&self.rects[i]) + self.gap)
                .fold(px(0.), |a, b| a + b);
            if target > self.from { span } else { -span }
        } else if self.from < slot && slot <= target {
            -self.shift()
        } else if target <= slot && slot < self.from {
            self.shift()
        } else {
            px(0.)
        }
    }

    /// The offset a slot should *start* this frame at so it slides into its new
    /// place instead of teleporting: where the last frame drew it, minus where
    /// this frame puts it. Zero for every slot the new target didn't disturb —
    /// which is all but one on a typical frame, so the list only animates the
    /// row you just crossed.
    pub(crate) fn flip_offset(&self, slot: usize, prev: usize, target: usize) -> Pixels {
        self.displacement(slot, prev) - self.displacement(slot, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    /// A vertical list of `n` slots, each `h` tall with a `gap` between them,
    /// starting at y = 0 — the sidebar's shape.
    fn column(n: usize, h: f32, gap: f32, from: usize) -> Reorder {
        let rects = (0..n)
            .map(|i| Bounds {
                origin: point(px(0.), px(i as f32 * (h + gap))),
                size: size(px(200.), px(h)),
            })
            .collect();
        Reorder::new(
            Surface::Strip,
            from,
            rects,
            Axis::Vertical,
            px(gap),
            // Grabbed dead centre of the slot.
            point(px(100.), px(h / 2.)),
        )
    }

    /// The dragged slot claims a new index the moment its centre reaches where
    /// the neighbour would sit without it, and holds its own index until then.
    #[test]
    fn target_follows_the_pointer_across_neighbours() {
        // 4 rows of 30px + 2px gaps: centres at 15, 47, 79, 111.
        let r = column(4, 30., 2., 0);
        // Dragging row 0, held by its centre. Row 1 yields once row 0's bottom
        // edge covers half of it — pointer 32, i.e. bottom edge at 47.
        assert_eq!(r.target(point(px(100.), px(32.))), 0);
        assert_eq!(r.target(point(px(100.), px(34.))), 1);
        // Then row 2's centre (79) at pointer 64, row 3's (111) at 96.
        assert_eq!(r.target(point(px(100.), px(66.))), 2);
        assert_eq!(r.target(point(px(100.), px(200.))), 3);
    }

    /// Dragging upward is the mirror image, and the order it previews is the
    /// dragged slot lifted out and re-inserted.
    #[test]
    fn order_lifts_the_dragged_slot_into_the_target() {
        let r = column(4, 30., 2., 3);
        assert_eq!(r.target(point(px(100.), px(2.))), 0);
        assert_eq!(r.order(0), vec![3, 0, 1, 2]);
        assert_eq!(r.order(1), vec![0, 3, 1, 2]);
        assert_eq!(r.order(3), vec![0, 1, 2, 3]);
    }

    /// Only the slot the drag just crossed gets a slide offset, and it's the
    /// full row pitch (row height + gap) in the direction it came from.
    #[test]
    fn flip_offset_animates_only_the_slot_just_crossed() {
        let r = column(4, 30., 2., 0);
        // Preview moved from "row 0 stays" to "row 0 sits after row 1":
        // row 1 closed up by one pitch, so it starts one pitch lower.
        assert_eq!(r.flip_offset(1, 0, 1), px(32.));
        // Rows the crossing didn't touch don't move at all.
        assert_eq!(r.flip_offset(2, 0, 1), px(0.));
        assert_eq!(r.flip_offset(3, 0, 1), px(0.));
        // The dragged slot slides too, now that it rides in the list rather
        // than floating over it: three rows crossed, three pitches to travel.
        assert_eq!(r.flip_offset(0, 0, 3), px(-96.));
        assert_eq!(r.flip_offset(0, 3, 0), px(96.));
        // Backing out again slides row 1 the other way.
        assert_eq!(r.flip_offset(1, 1, 0), px(-32.));
    }

    /// A tall block and a short one swap in *both* directions, with the same
    /// half-overlap threshold. The regression this pins: under a
    /// centre-to-centre test a three-row group could never move above a
    /// one-row group, because reaching its centre meant dragging the pointer
    /// off the top of the list.
    #[test]
    fn unequal_sizes_swap_in_both_directions() {
        // A 60px block at y=0 and a 140px block at y=62 (2px gap).
        let rects = vec![
            Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(200.), px(60.)),
            },
            Bounds {
                origin: point(px(0.), px(62.)),
                size: size(px(200.), px(140.)),
            },
        ];
        let grab = point(px(100.), px(10.));
        let tall = Reorder::new(
            Surface::SidebarGroups,
            1,
            rects.clone(),
            Axis::Vertical,
            px(2.),
            grab,
        );
        // Dragging the tall block up: it takes the top slot once its leading
        // edge passes the short block's centre (30) — pointer 40, i.e. ~30px
        // of travel from rest, all of it well inside the list.
        assert_eq!(tall.target(point(px(100.), px(41.))), 1);
        assert_eq!(tall.target(point(px(100.), px(39.))), 0);
        // Held at rest, it keeps its own slot.
        assert_eq!(tall.target(point(px(100.), px(72.))), 1);

        // And the short block still goes down past the tall one, at the same
        // half-overlap rule: trailing edge (pointer + 50) past centre 132.
        let short = Reorder::new(
            Surface::SidebarGroups,
            0,
            rects,
            Axis::Vertical,
            px(2.),
            grab,
        );
        assert_eq!(short.target(point(px(100.), px(80.))), 0);
        assert_eq!(short.target(point(px(100.), px(84.))), 1);
    }

    /// The held item tracks the pointer pixel for pixel, measured from
    /// whichever slot the list has currently laid it out in — so it sits under
    /// the cursor both before and after a crossing re-slots it.
    #[test]
    fn held_offset_tracks_the_pointer_across_a_crossing() {
        // 4 rows of 30px + 2px gaps (pitch 32), grabbed dead centre of row 0.
        let r = column(4, 30., 2., 0);
        // Nudged 10px down, still target 0: the row is 10px off its home slot.
        assert_eq!(r.held_offset(point(px(100.), px(25.)), 0), px(10.));
        // Just past row 1's centre the target flips to 1, and the row is now
        // laid out one pitch lower — so the same pointer reads 32px less.
        assert_eq!(r.held_offset(point(px(100.), px(48.)), 1), px(1.));
        // Dragging far past the end parks it against the last slot instead of
        // running off: row 3 starts at 96, so that's the furthest it goes.
        assert_eq!(r.held_offset(point(px(100.), px(900.)), 3), px(0.));
    }

    /// Only what the last drawn frame recorded may commit. The regression this
    /// pins: dragging an item away and then back to its own slot stops the
    /// surface recording (the move became a no-op), so without the per-frame
    /// clear the earlier "moved" order would survive and be applied on release,
    /// moving an item the user had visibly put back.
    #[test]
    fn pending_only_survives_the_frame_that_recorded_it() {
        let state: ReorderState = Rc::new(RefCell::new(Some(column(3, 30., 2., 0))));
        let mine = Surface::Strip;

        // A frame that previews a move records it.
        clear_pending(&state);
        set_pending(&state, &mine, vec![1, 0, 2]);
        // Another surface's recording is ignored — one drag, one owner.
        set_pending(&state, &Surface::SidebarGroups, vec![2, 1, 0]);

        // The next frame draws the item back in its own slot and records
        // nothing; the earlier order must not outlive it.
        clear_pending(&state);
        assert_eq!(take_pending(&state), None);
        // Taking also retires the drag.
        assert!(state.borrow().is_none());

        // And the ordinary path: recorded, then released.
        *state.borrow_mut() = Some(column(3, 30., 2., 0));
        clear_pending(&state);
        set_pending(&state, &mine, vec![1, 0, 2]);
        assert_eq!(take_pending(&state), Some(vec![1, 0, 2]));
    }

    /// The generation only advances when the preview actually changes, so a
    /// jittering cursor inside one slot doesn't restart the slide every frame.
    #[test]
    fn begin_frame_bumps_the_generation_only_on_change() {
        let r = column(3, 30., 2., 0);
        assert_eq!(r.begin_frame(0), (0, 0));
        assert_eq!(r.begin_frame(1), (1, 0));
        assert_eq!(r.begin_frame(1), (1, 1));
        assert_eq!(r.begin_frame(2), (2, 1));
    }

    /// A horizontal list measures along x — the strip's chips, which are wider
    /// than they are tall and vary in width.
    #[test]
    fn horizontal_lists_measure_along_x() {
        let widths = [100., 160., 120.];
        let mut x = 0.;
        let rects = widths
            .iter()
            .map(|w| {
                let b = Bounds {
                    origin: point(px(x), px(0.)),
                    size: size(px(*w), px(30.)),
                };
                x += w + 6.;
                b
            })
            .collect();
        let r = Reorder::new(
            Surface::Strip,
            0,
            rects,
            Axis::Horizontal,
            px(6.),
            point(px(50.), px(15.)),
        );
        // Chip 1's centre is at x=186; chip 0 takes its slot once its trailing
        // edge (pointer + 50) covers half of it.
        assert_eq!(r.target(point(px(135.), px(15.))), 0);
        assert_eq!(r.target(point(px(137.), px(15.))), 1);
        assert_eq!(r.order(2), vec![1, 2, 0]);
    }
}
