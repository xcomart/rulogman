//! Overlay scroll indicators.
//!
//! A thumb with no track behind it, drawn over the content rather than beside
//! it, shown while a surface is being scrolled or while the pointer rests on
//! the edge it rides, and taken down again once neither is true — the behaviour
//! macOS gives every scrollable view. Nothing here reserves layout space, so
//! turning it on costs a surface no width.
//!
//! While it is up the thumb can be dragged. Once it has gone, all that is left
//! along that edge is a sensor: a transparent strip that neither draws nor
//! occludes, so a press aimed at the content beneath goes straight through it,
//! and the only thing it does is notice a pointer arriving so the bar can come
//! back. That is the guarantee to hold on to — a bar that has gone shows
//! nothing and steals no press; it only listens.
//!
//! Four pieces, kept apart on purpose:
//!
//! * [`thumb`] and [`dragged_to`] are the geometry, and pure: offset to thumb,
//!   and pointer back to offset. Every awkward case — a surface with nothing to
//!   scroll, a thumb that ratio alone would shrink to a speck, a pointer
//!   dragged past either end — is decided there, where it can be tested without
//!   a window.
//! * [`ScrollbarState`] is the "is it showing?" flip-flop.  It carries no timer
//!   of its own: the owning view arms one with [`hide_later`], or starts the
//!   fade this instant with [`hide_now`], because only the view can notify
//!   itself when either lands.
//! * [`Scrollbar`] describes one bar, and both draws it and reads drags of it.
//! * [`DraggedThumb`] is what a drag of one carries.
//!
//! ## How a drag finds its way home
//!
//! gpui hands a `DragMoveEvent` to *every* element that listens for that drag
//! type, ancestor or not, and the `bounds` on the event are the listening
//! element's rather than the dragged one's. So the thumb carries its own track
//! — in window coordinates, as measured on the frame the drag began — inside
//! [`DraggedThumb`], along with where in the thumb the press landed. A listener
//! anywhere in the view can then map the pointer to an offset, and every bar's
//! drag is told apart from every other bar's by the id in the same payload.
//! Views therefore listen once, on their own root, and need no wiring around
//! each individual bar.

use std::cell::Cell;
use std::time::Duration;

use gpui::{Animation, transparent_black};
use gpui::{
    AnimationExt, AnyElement, App, Bounds, Context, DragMoveEvent, ElementId, Pixels, Point,
    ScrollHandle, Size, Window, div, ease_in_out, prelude::*, px,
};

use super::theme::Theme;

/// Thickness of the drawn thumb, in pixels.
///
/// Slim enough to sit over content without reading as a border; too slim to
/// aim at, which is what [`HIT_THICKNESS`] is for.
const THICKNESS: f32 = 4.;

/// Thickness of the area that answers a press, in pixels.
///
/// Wider than the thumb is drawn, because a four pixel target is not one a
/// pointer can be expected to find. Only the thickness is widened: the grab
/// area is exactly as long as the thumb, so the bare track stays untouched and
/// keeps letting presses through to the content under it.
///
/// The same thickness measures the strip that senses the pointer, so that the
/// band a pointer has to reach to summon the bar is the band it then has to
/// stay inside to hold it there.
const HIT_THICKNESS: f32 = 10.;

/// Default gap between the thumb and the container edges it rides.
pub const INSET: f32 = 2.;

/// Shortest the thumb may get, in pixels.
///
/// Length is otherwise the visible fraction of the content, which on a long
/// enough surface — a terminal with a full scrollback — would round to a speck
/// too small to read as a position, let alone to catch with a pointer.
const MIN_LENGTH: f32 = 24.;

/// How long the thumb stays up after the last movement.
///
/// Nothing announces the end of a scroll: movement is noticed one repaint at a
/// time, so a wheel turned in bursts and a wheel let go of look identical for
/// as long as the gap between two ticks. This is the width of that gap and
/// nothing more — long enough to carry the bar across it without a blink, short
/// enough that a bar nobody is scrolling or pointing at is on its way out
/// almost at once.
pub const SCROLL_LINGER: Duration = Duration::from_millis(500);

/// How long the thumb takes to appear.
///
/// Short: the bar is a reaction to something the user just did, and anything
/// slower reads as lag rather than as a fade.
pub const FADE_IN: Duration = Duration::from_millis(120);

/// How long the thumb takes to go away again.
///
/// Twice the fade in, and for the opposite reason: nothing is waiting on it, so
/// it can afford to leave gently rather than blink out.
pub const FADE_OUT: Duration = Duration::from_millis(250);

/// What a bar is doing right now.
///
/// The whole of "is it on screen, and how solidly?" — the owning view keeps one
/// of these per surface and the bar is drawn from it, so a bar mid-fade is a
/// state that can be reasoned about rather than a wall-clock guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fade {
    /// Not on screen: nothing of it is drawn, and nothing of it takes a press.
    #[default]
    Hidden,
    /// Coming up.
    In,
    /// Up, at full strength.
    Shown,
    /// Going away, and still there to be caught until it has gone.
    Out,
}

/// Which edge of its container a bar rides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarAxis {
    /// Along the bottom edge, for a surface that scrolls sideways.
    Horizontal,
    /// Down the right-hand edge, for a surface that scrolls up and down.
    Vertical,
}

impl ScrollbarAxis {
    /// The component of `size` that runs along this axis.
    fn along(self, size: Size<Pixels>) -> Pixels {
        match self {
            ScrollbarAxis::Horizontal => size.width,
            ScrollbarAxis::Vertical => size.height,
        }
    }

    /// The component of `point` that runs along this axis.
    fn of(self, point: Point<Pixels>) -> Pixels {
        match self {
            ScrollbarAxis::Horizontal => point.x,
            ScrollbarAxis::Vertical => point.y,
        }
    }
}

/// Where the thumb sits along its container's edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thumb {
    /// Distance from the container's leading edge to the start of the thumb.
    pub start: Pixels,
    /// Length of the thumb along the scrolling axis.
    pub length: Pixels,
}

/// The thumb for a surface `track` pixels long, or `None` when there is nothing
/// to scroll and so nothing to say.
///
/// `visible`, `scrollable` and `scrolled` are all in the *same* unit, whatever
/// it is — pixels for a gpui scroll container, lines for a terminal — because
/// only their ratios are used: `visible / (visible + scrollable)` sets the
/// length and `scrolled / scrollable` sets the position. `scrollable` is how
/// much lies beyond the visible part, which is exactly what
/// [`gpui::ScrollHandle::max_offset`] reports.
///
/// An offset past either end is clamped rather than refused: a surface is
/// briefly scrolled out of range whenever gpui applies a wheel delta, and is
/// pulled back on the next layout pass.
pub fn thumb(track: Pixels, visible: f32, scrollable: f32, scrolled: f32) -> Option<Thumb> {
    if !visible.is_finite() || !scrollable.is_finite() || !scrolled.is_finite() {
        return None;
    }
    if track <= px(0.) || visible <= 0. || scrollable <= 0. {
        return None;
    }

    let length = (track * (visible / (visible + scrollable)))
        .max(px(MIN_LENGTH))
        .min(track);
    let start = (track - length) * (scrolled / scrollable).clamp(0., 1.);

    Some(Thumb { start, length })
}

/// How far along its range a thumb dragged to `pointer` has reached, as a
/// fraction from `0.` at the start to `1.` at the end.
///
/// The inverse of [`thumb`], and the reason a drag needs no running total:
/// `pointer` is measured from the track's leading edge and `grab` is how far
/// into the thumb the press landed, so the same point of the thumb stays under
/// the pointer however far the gesture wanders, including outside the window.
///
/// `None` when the thumb fills its track, where there is nowhere to drag it and
/// the division would be by zero.
pub fn dragged_to(track: Pixels, length: Pixels, pointer: Pixels, grab: Pixels) -> Option<f32> {
    let travel = track - length;
    if travel <= px(0.) {
        return None;
    }

    let progress = f32::from(pointer - grab) / f32::from(travel);
    progress.is_finite().then(|| progress.clamp(0., 1.))
}

/// What a thumb drag carries with it.
///
/// Everything a listener needs to answer "where has this gone?" without having
/// seen the press: which bar it is, the track it runs in, and where the pointer
/// took hold. See the module docs for why it travels rather than being read off
/// the event.
pub struct DraggedThumb {
    /// The bar being dragged, so a view with several tells them apart.
    id: ElementId,
    /// The bar's axis, so the pointer is read along the right one.
    axis: ScrollbarAxis,
    /// The track in window coordinates, as measured when the drag began.
    track: Bounds<Pixels>,
    /// The thumb's length when the drag began.
    length: Pixels,
    /// How far into the thumb the press landed.
    ///
    /// gpui only offers this number to the closure that builds the drag
    /// preview, so that closure parks it here on the way past. A [`Cell`]
    /// rather than a plain field because the payload is only ever seen through
    /// a shared reference after that.
    grab: Cell<Pixels>,
}

impl DraggedThumb {
    /// The fraction of its range this drag has reached, if it belongs to the
    /// bar `id`.
    pub fn progress(&self, id: &ElementId, position: Point<Pixels>) -> Option<f32> {
        if self.id != *id {
            return None;
        }

        let pointer = self.axis.of(position) - self.axis.of(self.track.origin);
        dragged_to(
            self.axis.along(self.track.size),
            self.length,
            pointer,
            self.grab.get(),
        )
    }
}

/// Whether a surface's bar is showing, and for how much longer.
///
/// Movement is noticed by comparing offsets between renders rather than by
/// hooking every route that scrolls — a wheel, a keyboard, "scroll the active
/// tab into view", a window resize. Anything that moves a surface repaints it,
/// so the comparison catches all of them and nothing has to remember to
/// announce itself. Two things move nothing and must still keep the bar up: a
/// pointer held still on the thumb, which is what [`ScrollbarState::hold`] is
/// for, and a pointer resting on the edge the bar rides, which is what
/// [`ScrollbarState::hover_enter`] is for.
///
/// Those two are also the only reasons a bar waits at all. Scrolling has to be
/// waited out, because its end is never announced; a pointer leaving the edge
/// is announced the moment it happens, so that bar starts going at once.
#[derive(Debug, Default)]
pub struct ScrollbarState {
    /// Offset at the last look, or `None` before the first one.
    ///
    /// The first look never counts as movement, so a surface does not flash a
    /// bar at the moment it appears.
    seen: Option<f32>,
    /// Which showing this is. An expiry timer carries the epoch it was armed
    /// at and stands down if a later movement has replaced it, so a bar is
    /// never taken down by a timer belonging to an older burst of scrolling.
    epoch: u64,
    /// Whether a pointer is holding the thumb. No timer may fire while it is.
    held: bool,
    /// Whether a pointer is on the edge the bar rides. No timer may fire while
    /// it is either: the bar was asked for, and it stays until it is left.
    hovered: bool,
    phase: Fade,
}

impl ScrollbarState {
    /// A bar that is not showing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Notes where the surface is scrolled to now.
    ///
    /// Returns the epoch to arm an expiry timer with when the surface moved
    /// since the last look, and `None` when it sat still — or when a drag or a
    /// resting pointer has the bar, either of which is already holding it up
    /// and will say so itself when it lets go.
    pub fn moved(&mut self, scrolled: f32) -> Option<u64> {
        let moved = self.seen.is_some_and(|seen| seen != scrolled);
        self.seen = Some(scrolled);
        if !moved || self.held || self.hovered {
            return None;
        }

        self.show();
        Some(self.epoch)
    }

    /// Puts the bar up, and retires whatever timer was going to take it down.
    ///
    /// A bar caught on its way out comes straight back at full strength rather
    /// than fading in again from nothing: it never left, and restarting the fade
    /// would dip it to invisible on the way back up.
    fn show(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.phase = match self.phase {
            Fade::Hidden => Fade::In,
            Fade::Out => Fade::Shown,
            up => up,
        };
    }

    /// Keeps the bar up, at full strength, while a pointer holds the thumb.
    ///
    /// Deliberately arms nothing: a drag would otherwise leave a timer behind
    /// for every pointer move it made.
    pub fn hold(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.held = true;
        self.phase = Fade::Shown;
    }

    /// Takes note of a pointer arriving on the edge the bar rides, putting the
    /// bar up and keeping it there.
    ///
    /// A bar the pointer catches on its way out comes back at full strength for
    /// the same reason a scrolled one does, so this is [`ScrollbarState::show`]
    /// with the pointer remembered; the epoch it bumps retires whatever expiry
    /// was going to take the bar down.
    ///
    /// Returns whether anything changed, which is whether the view needs to be
    /// repainted.
    pub fn hover_enter(&mut self) -> bool {
        let before = (self.hovered, self.phase);
        self.hovered = true;
        self.show();
        before != (self.hovered, self.phase)
    }

    /// Takes note of the pointer leaving that edge again, returning the epoch to
    /// start the fade with.
    ///
    /// Nothing is waited out here: the pointer's departure is an event in its
    /// own right, unlike the end of a scroll, so there is nothing to bridge and
    /// the bar can start going the moment it happens.
    ///
    /// `None` when the bar should stay where it is: while the thumb is held,
    /// because letting go of it starts its own clock, and when the bar is off
    /// screen already.
    pub fn hover_leave(&mut self) -> Option<u64> {
        self.hovered = false;
        if self.held {
            return None;
        }

        self.epoch = self.epoch.wrapping_add(1);
        self.showing().then_some(self.epoch)
    }

    /// Lets go of the thumb, returning the epoch to arm the expiry with.
    ///
    /// `None` when nothing was holding it, which is every mouse button release
    /// that had nothing to do with a bar.
    pub fn release(&mut self) -> Option<u64> {
        if !self.held {
            return None;
        }
        self.held = false;
        self.epoch = self.epoch.wrapping_add(1);
        Some(self.epoch)
    }

    /// What the bar is doing, which is everything needed to draw it.
    pub fn fade(&self) -> Fade {
        self.phase
    }

    /// Whether the bar is on screen at all, fading or not.
    pub fn showing(&self) -> bool {
        self.phase != Fade::Hidden
    }

    /// Starts the bar fading out, unless a newer movement has since put it back
    /// up or a pointer is holding the thumb or resting on the edge.
    ///
    /// Leaves the epoch alone, so the same one carries on to [`ScrollbarState::finish`]:
    /// the fade and the expiry that started it are one expiry, and anything that
    /// interrupts the first interrupts the second.
    ///
    /// Returns whether anything changed, which is whether the view needs to be
    /// repainted.
    pub fn hide(&mut self, epoch: u64) -> bool {
        if self.held
            || self.hovered
            || self.epoch != epoch
            || matches!(self.phase, Fade::Hidden | Fade::Out)
        {
            return false;
        }
        self.phase = Fade::Out;
        true
    }

    /// Takes the faded-out bar off screen, and with it the last of the thumb
    /// there was to press.
    pub fn finish(&mut self, epoch: u64) -> bool {
        if self.held || self.epoch != epoch || self.phase != Fade::Out {
            return false;
        }
        self.phase = Fade::Hidden;
        true
    }
}

/// Arms the timer that takes a bar down once the scrolling has stopped.
///
/// The wait is [`SCROLL_LINGER`], which is what a stopped scroll has to be told
/// apart from a paused one by.
pub fn hide_later<V: 'static>(
    epoch: u64,
    cx: &mut Context<V>,
    pick: impl Fn(&mut V) -> Option<&mut ScrollbarState> + 'static,
) {
    hide_after(SCROLL_LINGER, epoch, cx, pick);
}

/// Starts a bar fading out this instant, and takes it off screen when the fade
/// has run.
///
/// For the departures that announce themselves — a pointer leaving the edge the
/// bar rides — where there is nothing to wait out and waiting would only leave
/// the bar sitting over content the user has moved on from.
///
/// The first half happens here rather than in a task, so the bar is already on
/// its way out in the frame the pointer left in; only the tail that finishes the
/// fade is spawned.
pub fn hide_now<V: 'static>(
    view: &mut V,
    epoch: u64,
    cx: &mut Context<V>,
    pick: impl Fn(&mut V) -> Option<&mut ScrollbarState> + 'static,
) {
    if !pick(view).is_some_and(|state| state.hide(epoch)) {
        return;
    }
    cx.notify();

    cx.spawn(async move |view, cx| {
        cx.background_executor().timer(FADE_OUT).await;
        view.update(cx, |view, cx| {
            if pick(view).is_some_and(|state| state.finish(epoch)) {
                cx.notify();
            }
        })
        .ok();
    })
    .detach();
}

/// Arms the timer that takes a bar down again, `delay` from now.
///
/// `pick` finds the state inside the view when the timer fires rather than
/// borrowing it now, because by then the surface it belongs to may be gone —
/// a closed session's file listing, say.
pub fn hide_after<V: 'static>(
    delay: Duration,
    epoch: u64,
    cx: &mut Context<V>,
    pick: impl Fn(&mut V) -> Option<&mut ScrollbarState> + 'static,
) {
    cx.spawn(async move |view, cx| {
        cx.background_executor().timer(delay).await;
        let fading = view
            .update(cx, |view, cx| {
                let fading = pick(view).is_some_and(|state| state.hide(epoch));
                if fading {
                    cx.notify();
                }
                fading
            })
            .unwrap_or(false);
        if !fading {
            return;
        }

        // One task rather than two, because the fade is the tail of this same
        // expiry: anything that interrupts the wait — a scroll, a hand on the
        // thumb — bumps the epoch, and both halves below check it.
        cx.background_executor().timer(FADE_OUT).await;
        view.update(cx, |view, cx| {
            if pick(view).is_some_and(|state| state.finish(epoch)) {
                cx.notify();
            }
        })
        .ok();
    })
    .detach();
}

/// Callback told `true` when the pointer reaches the edge a bar rides, and
/// `false` when it leaves again.
type HoverHandler = Box<dyn Fn(&bool, &mut Window, &mut App)>;

/// One overlay bar: where it is, how long its content is, and how to draw it.
///
/// Built afresh on every render from whatever the surface can report — a
/// [`ScrollHandle`] for a gpui scroll container, plain numbers for anything
/// else — and used both to draw the thumb and to read a drag of it, so the two
/// can never disagree about the geometry.
pub struct Scrollbar {
    id: ElementId,
    axis: ScrollbarAxis,
    track: Bounds<Pixels>,
    visible: f32,
    scrollable: f32,
    scrolled: f32,
    inset: f32,
    fade: Fade,
    on_hover: Option<HoverHandler>,
}

impl Scrollbar {
    /// A bar over a surface measured in whatever unit suits it.
    ///
    /// `track` is the box the thumb rides, in window coordinates — the box the
    /// bar is drawn against, which for a scroll container is the container
    /// itself. See [`thumb`] for what the other three mean.
    pub fn new(
        id: impl Into<ElementId>,
        axis: ScrollbarAxis,
        track: Bounds<Pixels>,
        visible: f32,
        scrollable: f32,
        scrolled: f32,
    ) -> Self {
        Self {
            id: id.into(),
            axis,
            track,
            visible,
            scrollable,
            scrolled,
            inset: INSET,
            fade: Fade::Shown,
            on_hover: None,
        }
    }

    /// A bar over a gpui scroll container, measured off its handle.
    ///
    /// The handle reports the bounds and the scrollable extent as of the last
    /// layout pass, so a bar trails a resize by one frame and corrects itself
    /// on the next — which is the frame the resize is drawn in.
    pub fn for_handle(
        id: impl Into<ElementId>,
        axis: ScrollbarAxis,
        handle: &ScrollHandle,
    ) -> Self {
        let track = handle.bounds();
        Self::new(
            id,
            axis,
            track,
            f32::from(axis.along(track.size)),
            f32::from(axis.along(handle.max_offset())),
            scrolled(handle, axis),
        )
    }

    /// Moves the bar further in from the edge it rides.
    ///
    /// For a surface with something else already pinned to that edge — the file
    /// panel's resize grip — which would otherwise take the presses aimed at
    /// the thumb, being drawn on top of it.
    pub fn inset(mut self, inset: f32) -> Self {
        self.inset = inset;
        self
    }

    /// Sets what the bar is doing, which decides whether it is drawn at all and
    /// how solidly.
    ///
    /// Comes from the [`ScrollbarState`] the owner keeps beside the surface, and
    /// defaults to [`Fade::Shown`] so a caller that wants a plain bar can leave
    /// it alone.
    pub fn fade(mut self, fade: Fade) -> Self {
        self.fade = fade;
        self
    }

    /// Called with `true` when the pointer reaches the edge the bar rides, and
    /// with `false` when it leaves again.
    ///
    /// The one thing a bar cannot answer for itself: whether the pointer being
    /// there should put the bar up is a question about the
    /// [`ScrollbarState`] the owner keeps, and only the owner can notify itself
    /// afterwards. A bar left without a listener senses nothing and comes and
    /// goes with the scrolling alone.
    pub fn on_hover(mut self, listener: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Some(Box::new(listener));
        self
    }

    /// The thumb as it stands, or `None` when there is nothing to scroll.
    pub fn thumb(&self) -> Option<Thumb> {
        thumb(
            self.axis.along(self.track.size),
            self.visible,
            self.scrollable,
            self.scrolled,
        )
    }

    /// The bar as an element, or `None` when there is nothing to scroll and so
    /// nothing to say.
    ///
    /// Absolutely positioned, so it is placed against its parent's padding box
    /// and takes no part in that parent's layout. The parent has to be the box
    /// the thumb measures — for a scroll container that means a wrapper around
    /// it, not the container itself, whose own children scroll away underneath.
    ///
    /// Four boxes. Outermost is the strip, running the whole length of the
    /// track and [`HIT_THICKNESS`] across, which draws nothing and answers
    /// nothing itself. Inside it sits the grab area, transparent and the same
    /// thickness but only as long as the thumb, and that one occludes: a press
    /// on the thumb belongs to the bar and must not also reach the tab, row or
    /// terminal underneath. Inside that is the [`THICKNESS`] the eye sees.
    /// Splitting the two is what lets the bar be as slim as it should look and
    /// still be worth aiming at — and only the grab area is widened, so the bare
    /// track keeps letting presses through to the content beneath it.
    ///
    /// The fourth is the sensor, which fills the strip, carries its hover
    /// listener, and is painted last on purpose. gpui's hit test walks the boxes
    /// from the front and stops at the first that occludes, so a sensor behind
    /// the grab area would drop out of the hit list the moment the pointer
    /// reached the thumb — and the bar would read that as the pointer having
    /// left. In front it stays in the list, and because it occludes nothing the
    /// grab area behind it stays there too.
    ///
    /// A [`Fade::Hidden`] bar is the strip and its sensor alone: nothing drawn,
    /// nothing to press, which is the whole of "what has gone cannot be caught".
    /// A fading one is still there to be caught, because it can still be seen —
    /// and catching it brings it back, since holding the thumb pins the bar at
    /// full strength.
    ///
    /// The fill is opaque on purpose. A translucent window composes one tint
    /// fill per pixel and no more (see `app_settings::window_tint`), and a bar
    /// over the terminal surface would be a second one; the fades are the one
    /// exception, and they are over in a quarter of a second.
    pub fn render(self, theme: &Theme) -> Option<AnyElement> {
        let thumb = self.thumb()?;

        let axis = self.axis;
        let track = self.track;
        let inset = self.inset;
        // Always deep enough to hold the thumb drawn inside it, however far in
        // from the edge that thumb has been pushed.
        let hit = px(HIT_THICKNESS.max(inset + THICKNESS));

        let drawn = (self.fade != Fade::Hidden).then(|| {
            let length = thumb.length;
            let grab = div()
                .id(self.id.clone())
                .absolute()
                .occlude()
                .bg(transparent_black())
                // An empty preview: the thumb follows the pointer directly, so a
                // ghost trailing it would only be a second thing to watch.
                .on_drag(
                    DraggedThumb {
                        id: self.id.clone(),
                        axis,
                        track,
                        length,
                        grab: Cell::new(px(0.)),
                    },
                    move |dragged, grab, _window, cx| {
                        dragged.grab.set(axis.of(grab));
                        cx.new(|_| gpui::Empty)
                    },
                );
            let seen = div().absolute().rounded_full().bg(theme.text_muted);

            let bar = match axis {
                ScrollbarAxis::Horizontal => grab
                    .left(thumb.start)
                    .bottom(px(0.))
                    .w(thumb.length)
                    .h(hit)
                    .child(seen.left_0().right_0().bottom(px(inset)).h(px(THICKNESS))),
                ScrollbarAxis::Vertical => grab
                    .top(thumb.start)
                    .right(px(0.))
                    .h(thumb.length)
                    .w(hit)
                    .child(seen.top_0().bottom_0().right(px(inset)).w(px(THICKNESS))),
            };

            // The animation drives its own frames — it asks for the next one
            // until it is done — so nothing here has to keep the view
            // repainting. Each phase animates under an id of its own, so that
            // entering one starts its fade from the beginning rather than
            // resuming the phase before. Only what is drawn is wrapped: the
            // sensor below has to be there through every phase, including the
            // one where there is nothing to see.
            match self.fade {
                Fade::In => bar
                    .with_animation(
                        ElementId::from((self.id.clone(), "fade-in")),
                        Animation::new(FADE_IN).with_easing(ease_in_out),
                        |bar, delta| bar.opacity(delta),
                    )
                    .into_any_element(),
                Fade::Out => bar
                    .with_animation(
                        ElementId::from((self.id.clone(), "fade-out")),
                        Animation::new(FADE_OUT).with_easing(ease_in_out),
                        |bar, delta| bar.opacity(1. - delta),
                    )
                    .into_any_element(),
                _ => bar.into_any_element(),
            }
        });

        // Carries no listener of its own beyond the hover one, and never
        // occludes: everything the strip covers but the thumb has to go on
        // answering presses exactly as it did before the bar was over it.
        let sensor = div()
            .id(ElementId::from((self.id.clone(), "hover")))
            .absolute()
            .when_some(self.on_hover, |sensor, listener| sensor.on_hover(listener));

        let strip = div().absolute().children(drawn);
        Some(
            match axis {
                ScrollbarAxis::Horizontal => strip
                    .left_0()
                    .right_0()
                    .bottom(px(0.))
                    .h(hit)
                    .child(sensor.left_0().right_0().top_0().bottom_0()),
                ScrollbarAxis::Vertical => strip
                    .top_0()
                    .bottom_0()
                    .right(px(0.))
                    .w(hit)
                    .child(sensor.left_0().right_0().top_0().bottom_0()),
            }
            .into_any_element(),
        )
    }

    /// How far along its range `event` has dragged this bar, or `None` when the
    /// drag belongs to another bar.
    pub fn dragged(&self, event: &DragMoveEvent<DraggedThumb>, cx: &App) -> Option<f32> {
        event.drag(cx).progress(&self.id, event.event.position)
    }
}

/// How far `handle` is scrolled along `axis`, counting up from the start.
///
/// gpui measures a scroll offset as the displacement of the content, which runs
/// negative as a surface scrolls; this is the same distance the other way
/// round, which is what a bar is positioned by.
pub fn scrolled(handle: &ScrollHandle, axis: ScrollbarAxis) -> f32 {
    f32::from(match axis {
        ScrollbarAxis::Horizontal => -handle.offset().x,
        ScrollbarAxis::Vertical => -handle.offset().y,
    })
}

/// Scrolls `handle` to `progress` of its range, reporting whether it moved.
///
/// Written straight into the offset gpui itself scrolls with, and left for
/// gpui's next layout pass to pin to the scrollable range, exactly as a wheel
/// delta is.
pub fn scroll_to(handle: &ScrollHandle, axis: ScrollbarAxis, progress: f32) -> bool {
    let scrollable = axis.along(handle.max_offset());
    let mut offset = handle.offset();
    match axis {
        ScrollbarAxis::Horizontal => offset.x = -(scrollable * progress),
        ScrollbarAxis::Vertical => offset.y = -(scrollable * progress),
    }

    if offset == handle.offset() {
        return false;
    }
    handle.set_offset(offset);
    true
}

/// Keeps a wheel on the axis it was turned along.
///
/// gpui's own scroll listener folds a wheel's delta on the axis a container
/// doesn't scroll onto the one it does, unless told otherwise — so a sideways
/// wheel over a vertical-only list (or a vertical wheel over a horizontal-only
/// strip) drags it along the axis it never asked to move on. This opts a
/// container out, so the two axes stay independent. Every vertical-only
/// surface in the app uses it; `tab_bar`'s horizontal strip deliberately
/// leaves it off so a vertical wheel still drives its sideways scroll.
pub trait WheelStaysOnAxis: InteractiveElement + Sized {
    /// Restricts this element's wheel scrolling to the axis it actually
    /// scrolls on, so a wheel turned the other way is ignored instead of
    /// being folded onto this axis.
    fn wheel_stays_on_axis(mut self) -> Self {
        self.interactivity().base_style.restrict_scroll_to_axis = Some(true);
        self
    }
}

impl<E: InteractiveElement> WheelStaysOnAxis for E {}

#[cfg(test)]
mod tests {
    use gpui::{point, size};

    use super::*;

    /// A track a hundred long, starting at the window's origin.
    fn track(length: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), size(px(length), px(length)))
    }

    /// A surface that fits has nothing to point at.
    #[test]
    fn a_surface_with_nothing_to_scroll_has_no_thumb() {
        assert_eq!(thumb(px(100.), 100., 0., 0.), None);
    }

    /// And neither has one that has not been laid out yet, or one whose
    /// geometry arrived as a nonsense float.
    #[test]
    fn an_unmeasurable_surface_has_no_thumb() {
        assert_eq!(thumb(px(0.), 100., 100., 0.), None);
        assert_eq!(thumb(px(-10.), 100., 100., 0.), None);
        assert_eq!(thumb(px(100.), 0., 100., 0.), None);
        assert_eq!(thumb(px(100.), f32::NAN, 100., 0.), None);
        assert_eq!(thumb(px(100.), 100., f32::INFINITY, 0.), None);
    }

    /// Length is the visible share of the whole, and the ends line up: at rest
    /// the thumb starts at the top, at the end it finishes at the bottom.
    #[test]
    fn the_thumb_spans_the_visible_share_and_reaches_both_ends() {
        let top = thumb(px(200.), 100., 300., 0.).expect("a scrollable surface");
        assert_eq!(top.length, px(50.));
        assert_eq!(top.start, px(0.));

        let bottom = thumb(px(200.), 100., 300., 300.).expect("a scrollable surface");
        assert_eq!(bottom.length, px(50.));
        assert_eq!(bottom.start + bottom.length, px(200.));

        let middle = thumb(px(200.), 100., 300., 150.).expect("a scrollable surface");
        assert_eq!(middle.start, px(75.));
    }

    /// A very long surface still gets a thumb that can be seen and caught, and
    /// it still reaches the far end rather than running off it.
    #[test]
    fn a_long_surface_gets_a_thumb_that_is_still_visible() {
        let short = thumb(px(200.), 10., 100_000., 0.).expect("a scrollable surface");
        assert_eq!(short.length, px(MIN_LENGTH));

        let end = thumb(px(200.), 10., 100_000., 100_000.).expect("a scrollable surface");
        assert_eq!(end.start + end.length, px(200.));
    }

    /// A thumb never outgrows its track, however the ratios come out.
    #[test]
    fn the_thumb_never_outgrows_its_track() {
        let tiny = thumb(px(10.), 100., 1., 0.).expect("a scrollable surface");
        assert_eq!(tiny.length, px(10.));
        assert_eq!(tiny.start, px(0.));
    }

    /// gpui lets an offset run past the end between a wheel and the layout pass
    /// that pins it back. The thumb stays on its track meanwhile.
    #[test]
    fn an_offset_past_either_end_is_pinned_to_the_track() {
        let past = thumb(px(200.), 100., 300., 900.).expect("a scrollable surface");
        assert_eq!(past.start + past.length, px(200.));

        let before = thumb(px(200.), 100., 300., -900.).expect("a scrollable surface");
        assert_eq!(before.start, px(0.));
    }

    /// A drag reads back exactly what drew the thumb: grab it where it sits,
    /// move nowhere, and the surface has not scrolled.
    #[test]
    fn a_drag_that_goes_nowhere_scrolls_nothing() {
        let thumb = thumb(px(200.), 100., 300., 150.).expect("a scrollable surface");

        let progress = dragged_to(px(200.), thumb.length, thumb.start + px(20.), px(20.))
            .expect("a thumb with room to travel");
        assert_eq!(progress, 0.5);
    }

    /// And the point taken hold of stays under the pointer: the same grab a
    /// third of the way down the thumb lands the thumb's start a third short of
    /// the pointer, wherever the pointer goes.
    #[test]
    fn a_drag_keeps_the_grabbed_point_under_the_pointer() {
        let progress =
            dragged_to(px(200.), px(50.), px(100.), px(10.)).expect("a thumb with room to travel");
        assert_eq!(progress, 0.6);

        let start = (px(200.) - px(50.)) * progress;
        assert_eq!(start + px(10.), px(100.));
    }

    /// Dragged past either end, the surface stops at that end rather than
    /// running on or wrapping round.
    #[test]
    fn a_drag_past_either_end_stops_there() {
        assert_eq!(dragged_to(px(200.), px(50.), px(9_000.), px(10.)), Some(1.));
        assert_eq!(
            dragged_to(px(200.), px(50.), px(-9_000.), px(10.)),
            Some(0.)
        );
    }

    /// A thumb that fills its track has nowhere to go, and says so rather than
    /// dividing by zero.
    #[test]
    fn a_thumb_that_fills_its_track_cannot_be_dragged() {
        assert_eq!(dragged_to(px(200.), px(200.), px(50.), px(10.)), None);
        assert_eq!(dragged_to(px(200.), px(300.), px(50.), px(10.)), None);
    }

    /// A drag only answers to the bar it started on. Two bars in one view see
    /// each other's moves, and must ignore them.
    #[test]
    fn a_drag_answers_only_to_its_own_bar() {
        let dragged = DraggedThumb {
            id: "mine".into(),
            axis: ScrollbarAxis::Vertical,
            track: track(200.),
            length: px(50.),
            grab: Cell::new(px(10.)),
        };

        assert_eq!(
            dragged.progress(&"mine".into(), point(px(0.), px(100.))),
            Some(0.6)
        );
        assert_eq!(
            dragged.progress(&"theirs".into(), point(px(0.), px(100.))),
            None
        );
    }

    /// The pointer is read along the bar's own axis, and against the track's
    /// own corner rather than the window's.
    #[test]
    fn a_drag_is_read_along_its_axis_from_the_track_corner() {
        let offset = Bounds::new(point(px(40.), px(80.)), size(px(200.), px(200.)));
        let sideways = DraggedThumb {
            id: "bar".into(),
            axis: ScrollbarAxis::Horizontal,
            track: offset,
            length: px(50.),
            grab: Cell::new(px(10.)),
        };

        // 140 in the window is 100 along a track that starts at 40.
        assert_eq!(
            sideways.progress(&"bar".into(), point(px(140.), px(9_999.))),
            Some(0.6)
        );
    }

    /// The first look is not movement, or every surface would flash a bar at
    /// the moment it is first drawn.
    #[test]
    fn the_first_look_at_a_surface_shows_nothing() {
        let mut state = ScrollbarState::new();

        assert_eq!(state.moved(0.), None);
        assert_eq!(state.fade(), Fade::Hidden);
        assert!(!state.showing());
    }

    /// Movement fades the bar in; sitting still leaves it as it was; the expiry
    /// fades it out and only then takes it off screen.
    #[test]
    fn a_bar_fades_in_waits_and_fades_out_again() {
        let mut state = ScrollbarState::new();
        state.moved(0.);

        let epoch = state.moved(40.).expect("a moved surface");
        assert_eq!(state.fade(), Fade::In);

        assert_eq!(state.moved(40.), None);
        assert_eq!(
            state.fade(),
            Fade::In,
            "sitting still cut the fade in short"
        );

        assert!(state.hide(epoch));
        assert_eq!(state.fade(), Fade::Out);
        assert!(state.showing(), "a fading bar has to still be drawn");

        assert!(state.finish(epoch));
        assert_eq!(state.fade(), Fade::Hidden);
        assert!(!state.showing());
    }

    /// Scrolling again while the bar is on its way out brings it straight back
    /// at full strength — not back to the start of a fade in, which would dip
    /// it to invisible on the way up.
    #[test]
    fn scrolling_during_the_fade_out_brings_the_bar_straight_back() {
        let mut state = ScrollbarState::new();
        state.moved(0.);
        let going = state.moved(40.).expect("a moved surface");
        assert!(state.hide(going));
        assert_eq!(state.fade(), Fade::Out);

        let back = state.moved(80.).expect("a moved surface");
        assert_eq!(state.fade(), Fade::Shown);
        assert_ne!(back, going);

        // And the fade it interrupted cannot finish behind its back.
        assert!(!state.finish(going), "an interrupted fade still completed");
        assert_eq!(state.fade(), Fade::Shown);
    }

    /// A timer armed by an earlier burst of scrolling cannot touch the bar a
    /// later one put up.
    #[test]
    fn a_stale_timer_leaves_a_newer_showing_alone() {
        let mut state = ScrollbarState::new();
        state.moved(0.);

        let stale = state.moved(40.).expect("a moved surface");
        let fresh = state.moved(80.).expect("a moved surface");
        assert_ne!(stale, fresh);

        assert!(!state.hide(stale), "a stale timer hid a newer showing");
        assert_eq!(state.fade(), Fade::In);

        assert!(state.hide(fresh));
        assert_eq!(state.fade(), Fade::Out);
    }

    /// And a timer that fires against a bar already going or gone changes
    /// nothing, so it asks for no repaint.
    #[test]
    fn hiding_a_bar_that_is_already_going_changes_nothing() {
        let mut state = ScrollbarState::new();
        state.moved(0.);
        let epoch = state.moved(40.).expect("a moved surface");

        assert!(state.hide(epoch));
        assert!(!state.hide(epoch), "a bar was faded out twice over");
        assert!(state.finish(epoch));
        assert!(!state.finish(epoch));
        assert!(!state.hide(epoch));
    }

    /// A pointer holding the thumb keeps the bar at full strength however long
    /// it is held still, and no timer armed before or during the hold may fade
    /// it — including one that had already started the fade.
    #[test]
    fn a_held_thumb_keeps_the_bar_at_full_strength() {
        let mut state = ScrollbarState::new();
        state.moved(0.);
        let before = state.moved(40.).expect("a moved surface");
        assert!(state.hide(before));
        assert_eq!(state.fade(), Fade::Out);

        // Caught on the way out: the hold is what cancels the fade.
        state.hold();
        assert_eq!(state.fade(), Fade::Shown);
        assert!(!state.hide(before), "a timer fired through a held thumb");
        assert!(!state.finish(before), "a fade completed under a held thumb");

        // Movement during the hold arms nothing, so it leaves no timer behind
        // for every pixel the pointer travelled.
        assert_eq!(state.moved(80.), None);
        assert_eq!(state.fade(), Fade::Shown);

        let epoch = state.release().expect("a held thumb");
        assert_eq!(state.fade(), Fade::Shown, "letting go blinked the bar out");
        assert!(state.hide(epoch));
        assert!(state.finish(epoch));
        assert_eq!(state.fade(), Fade::Hidden);
    }

    /// A release that had no thumb to let go of asks for no timer, which is
    /// every other mouse button release the view sees.
    #[test]
    fn releasing_nothing_arms_nothing() {
        let mut state = ScrollbarState::new();

        assert_eq!(state.release(), None);
    }

    /// A bar the pointer reaches comes up without anything having scrolled, and
    /// stays up for as long as the pointer is on it.
    #[test]
    fn a_pointer_on_the_edge_puts_the_bar_up() {
        let mut state = ScrollbarState::new();
        state.moved(0.);
        assert_eq!(state.fade(), Fade::Hidden);

        assert!(state.hover_enter(), "arriving asked for no repaint");
        assert_eq!(state.fade(), Fade::In);
        assert!(
            !state.hover_enter(),
            "staying put asked for a repaint anyway"
        );
    }

    /// And one the pointer catches on its way out comes back at full strength,
    /// for the same reason scrolling into a fade does.
    #[test]
    fn a_pointer_catches_a_fading_bar_at_full_strength() {
        let mut state = ScrollbarState::new();
        state.moved(0.);
        let going = state.moved(40.).expect("a moved surface");
        assert!(state.hide(going));
        assert_eq!(state.fade(), Fade::Out);

        assert!(state.hover_enter());
        assert_eq!(state.fade(), Fade::Shown);
        assert!(!state.finish(going), "an interrupted fade still completed");
    }

    /// The timer a scroll leaves behind cannot take down a bar the pointer is
    /// resting on, however long it has been resting there.
    #[test]
    fn a_resting_pointer_outlasts_the_scrolls_timer() {
        let mut state = ScrollbarState::new();
        state.moved(0.);
        let scrolling = state.moved(40.).expect("a moved surface");

        state.hover_enter();
        assert!(
            !state.hide(scrolling),
            "a timer hid a bar under the pointer"
        );

        // Movement under a resting pointer arms nothing, so a wheel turned over
        // the edge leaves no timer behind for every tick of it.
        assert_eq!(state.moved(80.), None);
        assert!(state.showing());
    }

    /// The pointer leaving is the whole of the notice needed: the bar starts
    /// going at once, under the epoch the departure hands back.
    #[test]
    fn the_pointer_leaving_starts_the_fade_at_once() {
        let mut state = ScrollbarState::new();
        state.moved(0.);
        state.hover_enter();

        let epoch = state.hover_leave().expect("a bar that was up");
        assert!(state.hide(epoch));
        assert_eq!(state.fade(), Fade::Out);
        assert!(state.finish(epoch));
        assert_eq!(state.fade(), Fade::Hidden);

        // And a pointer leaving a bar that has already gone asks for nothing,
        // there being no fade left to start.
        state.hover_enter();
        let again = state.hover_leave().expect("a bar that was up");
        state.hide(again);
        state.finish(again);
        assert_eq!(state.hover_leave(), None);
    }

    /// Arriving retires whatever was going to take the bar down, so the timer
    /// the last scroll armed cannot fire through the pointer that followed it.
    #[test]
    fn a_pointer_arriving_retires_the_scrolls_timer() {
        let mut state = ScrollbarState::new();
        state.moved(0.);
        let scrolling = state.moved(40.).expect("a moved surface");

        state.hover_enter();
        let leaving = state.hover_leave().expect("a bar that was up");
        assert_ne!(leaving, scrolling);
        assert!(!state.hide(scrolling), "a retired timer hid the bar anyway");
    }

    /// A pointer that leaves while the thumb is being dragged asks for nothing:
    /// a drag that carries the thumb off the edge the pointer started on has not
    /// finished with the bar, and letting go is what starts its clock.
    #[test]
    fn a_pointer_leaving_a_held_thumb_asks_for_nothing() {
        let mut state = ScrollbarState::new();
        state.moved(0.);
        state.hover_enter();
        state.hold();

        assert_eq!(state.hover_leave(), None);
        assert_eq!(state.fade(), Fade::Shown);

        let epoch = state.release().expect("a held thumb");
        assert!(state.hide(epoch));
        assert!(state.finish(epoch));
        assert_eq!(state.fade(), Fade::Hidden);
    }

    /// A bar that is off screen draws nothing, but the strip that senses the
    /// pointer is still there — that is what lets a bar nobody has scrolled come
    /// back at all. A surface with nothing to scroll has neither.
    #[test]
    fn a_hidden_bar_leaves_only_its_sensor() {
        let bar = Scrollbar::new("bar", ScrollbarAxis::Vertical, track(200.), 100., 300., 0.);
        assert!(bar.thumb().is_some(), "the surface had nothing to scroll");
        assert!(
            bar.fade(Fade::Hidden).render(&Theme::dark()).is_some(),
            "a hidden bar left nothing to sense the pointer with"
        );

        let nothing = Scrollbar::new("bar", ScrollbarAxis::Vertical, track(200.), 100., 0., 0.);
        assert!(nothing.thumb().is_none());
        assert!(
            nothing.render(&Theme::dark()).is_none(),
            "a surface with nothing to scroll was drawn anyway"
        );
    }
}
