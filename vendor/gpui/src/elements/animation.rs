use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use crate::{
    AnyElement, App, Element, ElementId, GlobalElementId, Hsla, InspectorElementId,
    InteractiveElement, Interactivity, IntoElement, Pixels, Point, StyleRefinement, Styled, Window,
    hsla,
};

pub use easing::*;
use smallvec::SmallVec;

/// An animation that can be applied to an element.
#[derive(Clone)]
pub struct Animation {
    /// The amount of time for which this animation should run
    pub duration: Duration,
    /// How long to wait before the animation starts.
    pub delay: Duration,
    /// Whether to repeat this animation when it finishes
    pub oneshot: bool,
    /// A function that takes a delta between 0 and 1 and returns a new delta
    /// between 0 and 1 based on the given easing function.
    pub easing: Rc<dyn Fn(f32) -> f32>,
}

impl Animation {
    /// Create a new animation with the given duration.
    /// By default the animation will only run once and will use a linear easing function.
    pub fn new(duration: Duration) -> Self {
        Self {
            duration,
            delay: Duration::ZERO,
            oneshot: true,
            easing: Rc::new(linear),
        }
    }

    /// Delay the start of this animation.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Set the animation to loop when it finishes.
    pub fn repeat(mut self) -> Self {
        self.oneshot = false;
        self
    }

    /// Set the easing function to use for this animation.
    /// The easing function will take a time delta between 0 and 1 and return a new delta
    /// between 0 and 1
    pub fn with_easing(mut self, easing: impl Fn(f32) -> f32 + 'static) -> Self {
        self.easing = Rc::new(easing);
        self
    }
}

/// An extension trait for adding the animation wrapper to both Elements and Components
pub trait AnimationExt {
    /// Render this component or element with an animation
    fn with_animation(
        self,
        id: impl Into<ElementId>,
        animation: Animation,
        animator: impl Fn(Self, f32) -> Self + 'static,
    ) -> AnimationElement<Self>
    where
        Self: Sized,
    {
        AnimationElement {
            id: id.into(),
            element: Some(self),
            animator: Box::new(move |this, _, value| animator(this, value)),
            animations: smallvec::smallvec![animation],
            offsetter: None,
        }
    }

    /// Render this component or element with a chain of animations
    fn with_animations(
        self,
        id: impl Into<ElementId>,
        animations: Vec<Animation>,
        animator: impl Fn(Self, usize, f32) -> Self + 'static,
    ) -> AnimationElement<Self>
    where
        Self: Sized,
    {
        AnimationElement {
            id: id.into(),
            element: Some(self),
            animator: Box::new(animator),
            animations: animations.into(),
            offsetter: None,
        }
    }
}

impl<E: IntoElement + 'static> AnimationExt for E {}

/// A transition that can interpolate style changes over time.
#[derive(Clone)]
pub struct Transition {
    animation: Animation,
}

impl Transition {
    /// Create a transition with a linear easing curve.
    pub fn new(duration: Duration) -> Self {
        Self {
            animation: Animation::new(duration),
        }
    }

    /// Set the easing function to use for this transition.
    ///
    /// The easing function takes a time delta between 0 and 1 and returns a new
    /// delta between 0 and 1.
    pub fn with_easing(mut self, easing: impl Fn(f32) -> f32 + 'static) -> Self {
        self.animation = self.animation.with_easing(easing);
        self
    }

    /// Delay the start of this transition.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.animation = self.animation.with_delay(delay);
        self
    }

    /// Return the underlying animation.
    pub fn into_animation(self) -> Animation {
        self.animation
    }
}

impl From<Duration> for Transition {
    fn from(duration: Duration) -> Self {
        Self::new(duration)
    }
}

impl From<Animation> for Transition {
    fn from(animation: Animation) -> Self {
        Self { animation }
    }
}

/// A value that can be interpolated during a transition.
pub trait TransitionValue: Copy {
    /// Interpolate from `self` to `target` at `delta`, where `delta` is expected
    /// to be between 0 and 1.
    fn interpolate(self, target: Self, delta: f32) -> Self;
}

impl TransitionValue for f32 {
    fn interpolate(self, target: Self, delta: f32) -> Self {
        self + (target - self) * delta.clamp(0.0, 1.0)
    }
}

impl TransitionValue for Pixels {
    fn interpolate(self, target: Self, delta: f32) -> Self {
        self + (target - self) * delta.clamp(0.0, 1.0)
    }
}

impl TransitionValue for Point<Pixels> {
    fn interpolate(self, target: Self, delta: f32) -> Self {
        Point {
            x: self.x.interpolate(target.x, delta),
            y: self.y.interpolate(target.y, delta),
        }
    }
}

impl TransitionValue for Hsla {
    fn interpolate(self, target: Self, delta: f32) -> Self {
        hsla(
            self.h.interpolate(target.h, delta),
            self.s.interpolate(target.s, delta),
            self.l.interpolate(target.l, delta),
            self.a.interpolate(target.a, delta),
        )
    }
}

/// Interpolate a value from `from` to `to` at `delta`.
pub fn transition_value<T: TransitionValue>(from: T, to: T, delta: f32) -> T {
    from.interpolate(to, delta)
}

/// Ergonomic transition helpers for styled elements.
pub trait TransitionExt: Styled + IntoElement + Sized + 'static {
    /// Render this element with a transition that can update its style as time advances.
    fn with_transition(
        self,
        id: impl Into<ElementId>,
        transition: impl Into<Transition>,
        animator: impl Fn(Self, f32) -> Self + 'static,
    ) -> AnimationElement<Self> {
        self.with_animation(id, transition.into().into_animation(), animator)
    }

    /// Transition the element opacity from `from` to `to`.
    fn transition_opacity(
        self,
        id: impl Into<ElementId>,
        transition: impl Into<Transition>,
        from: f32,
        to: f32,
    ) -> AnimationElement<Self> {
        self.with_transition(id, transition, move |element, delta| {
            element.opacity(from.interpolate(to, delta))
        })
    }

    /// Transition the element background color from `from` to `to`.
    fn transition_bg(
        self,
        id: impl Into<ElementId>,
        transition: impl Into<Transition>,
        from: Hsla,
        to: Hsla,
    ) -> AnimationElement<Self> {
        self.with_transition(id, transition, move |element, delta| {
            element.bg(from.interpolate(to, delta))
        })
    }

    /// Transition the element text color from `from` to `to`.
    fn transition_text_color(
        self,
        id: impl Into<ElementId>,
        transition: impl Into<Transition>,
        from: Hsla,
        to: Hsla,
    ) -> AnimationElement<Self> {
        self.with_transition(id, transition, move |element, delta| {
            element.text_color(from.interpolate(to, delta))
        })
    }

    /// Transition the element paint offset from `from` to `to`.
    ///
    /// The element keeps its measured layout; only its painted position and
    /// hitboxes move during prepaint. Use this for entrance motion rather than
    /// layout animation.
    fn transition_offset(
        self,
        id: impl Into<ElementId>,
        transition: impl Into<Transition>,
        from: Point<Pixels>,
        to: Point<Pixels>,
    ) -> AnimationElement<Self> {
        self.with_transition(id, transition, |element, _| element)
            .with_offset(move |_, delta| from.interpolate(to, delta))
    }

    /// Transition opacity and paint offset together.
    fn transition_opacity_and_offset(
        self,
        id: impl Into<ElementId>,
        transition: impl Into<Transition>,
        opacity_from: f32,
        opacity_to: f32,
        offset_from: Point<Pixels>,
        offset_to: Point<Pixels>,
    ) -> AnimationElement<Self> {
        self.transition_opacity(id, transition, opacity_from, opacity_to)
            .with_offset(move |_, delta| offset_from.interpolate(offset_to, delta))
    }
}

impl<E> TransitionExt for E where E: Styled + IntoElement + 'static {}

/// A GPUI element that applies an animation to another element
pub struct AnimationElement<E> {
    id: ElementId,
    element: Option<E>,
    animations: SmallVec<[Animation; 1]>,
    animator: Box<dyn Fn(E, usize, f32) -> E + 'static>,
    offsetter: Option<Box<dyn Fn(usize, f32) -> Point<Pixels> + 'static>>,
}

impl<E> AnimationElement<E> {
    /// Returns a new [`AnimationElement<E>`] after applying the given function
    /// to the element being animated.
    pub fn map_element(mut self, f: impl FnOnce(E) -> E) -> AnimationElement<E> {
        self.element = self.element.map(f);
        self
    }

    /// Paint this animation at a per-frame offset without changing layout.
    pub fn with_offset(
        mut self,
        offsetter: impl Fn(usize, f32) -> Point<Pixels> + 'static,
    ) -> AnimationElement<E> {
        self.offsetter = Some(Box::new(offsetter));
        self
    }
}

impl<E: IntoElement + 'static> IntoElement for AnimationElement<E> {
    type Element = AnimationElement<E>;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: Styled> Styled for AnimationElement<E> {
    fn style(&mut self) -> &mut StyleRefinement {
        self.element
            .as_mut()
            .expect("animation element should still be configurable")
            .style()
    }
}

impl<E: InteractiveElement> InteractiveElement for AnimationElement<E> {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.element
            .as_mut()
            .expect("animation element should still be configurable")
            .interactivity()
    }
}

struct AnimationState {
    start: Instant,
    animation_ix: usize,
}

/// Layout state for an animated element.
pub struct AnimationLayoutState {
    element: AnyElement,
    offset: Option<Point<Pixels>>,
}

impl<E: IntoElement + 'static> Element for AnimationElement<E> {
    type RequestLayoutState = AnimationLayoutState;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (crate::LayoutId, Self::RequestLayoutState) {
        window.with_element_state(global_id.unwrap(), |state, window| {
            let mut state = state.unwrap_or_else(|| AnimationState {
                start: Instant::now(),
                animation_ix: 0,
            });
            let animation_ix = state.animation_ix;

            let elapsed = state.start.elapsed();
            let animation = &self.animations[animation_ix];
            let mut delta = if elapsed < animation.delay {
                0.0
            } else {
                (elapsed - animation.delay).as_secs_f32() / animation.duration.as_secs_f32()
            };

            let mut done = false;
            if delta > 1.0 {
                if animation.oneshot {
                    if animation_ix >= self.animations.len() - 1 {
                        done = true;
                    } else {
                        state.start = Instant::now();
                        state.animation_ix += 1;
                    }
                    delta = 1.0;
                } else {
                    delta %= 1.0;
                }
            }
            let delta = (self.animations[animation_ix].easing)(delta);

            debug_assert!(
                (0.0..=1.0).contains(&delta),
                "delta should always be between 0 and 1"
            );

            let element = self.element.take().expect("should only be called once");
            let mut element = (self.animator)(element, animation_ix, delta).into_any_element();
            let offset = self
                .offsetter
                .as_ref()
                .map(|offsetter| offsetter(animation_ix, delta));

            if !done {
                window.request_animation_frame();
            }

            (
                (
                    element.request_layout(window, cx),
                    AnimationLayoutState { element, offset },
                ),
                state,
            )
        })
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: crate::Bounds<crate::Pixels>,
        element: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if let Some(offset) = element.offset {
            window.with_element_offset(offset, |window| element.element.prepaint(window, cx));
        } else {
            element.element.prepaint(window, cx);
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: crate::Bounds<crate::Pixels>,
        element: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        element.element.paint(window, cx);
    }
}

mod easing {
    use std::f32::consts::PI;

    /// The linear easing function, or delta itself
    pub fn linear(delta: f32) -> f32 {
        delta
    }

    /// The quadratic easing function, delta * delta
    pub fn quadratic(delta: f32) -> f32 {
        delta * delta
    }

    /// The quadratic ease-in-out function, which starts and ends slowly but speeds up in the middle
    pub fn ease_in_out(delta: f32) -> f32 {
        if delta < 0.5 {
            2.0 * delta * delta
        } else {
            let x = -2.0 * delta + 2.0;
            1.0 - x * x / 2.0
        }
    }

    /// The Quint ease-out function, which starts quickly and decelerates to a stop
    pub fn ease_out_quint() -> impl Fn(f32) -> f32 {
        move |delta| 1.0 - (1.0 - delta).powi(5)
    }

    /// Apply the given easing function, first in the forward direction and then in the reverse direction
    pub fn bounce(easing: impl Fn(f32) -> f32) -> impl Fn(f32) -> f32 {
        move |delta| {
            if delta < 0.5 {
                easing(delta * 2.0)
            } else {
                easing((1.0 - delta) * 2.0)
            }
        }
    }

    /// A custom easing function for pulsating alpha that slows down as it approaches 0.1
    pub fn pulsating_between(min: f32, max: f32) -> impl Fn(f32) -> f32 {
        let range = max - min;

        move |delta| {
            // Use a combination of sine and cubic functions for a more natural breathing rhythm
            let t = (delta * 2.0 * PI).sin();
            let breath = (t * t * t + t) / 2.0;

            // Map the breath to our desired alpha range
            let normalized_alpha = (breath + 1.0) / 2.0;

            min + (normalized_alpha * range)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::px;

    #[test]
    fn transition_values_are_clamped() {
        assert_eq!(transition_value(10.0, 20.0, -1.0), 10.0);
        assert_eq!(transition_value(10.0, 20.0, 0.25), 12.5);
        assert_eq!(transition_value(10.0, 20.0, 2.0), 20.0);
    }

    #[test]
    fn transition_colors_interpolate_components() {
        let from = hsla(0.0, 0.2, 0.4, 0.6);
        let to = hsla(1.0, 0.4, 0.6, 0.8);
        let color = transition_value(from, to, 0.5);

        assert!((color.h - 0.5).abs() < f32::EPSILON);
        assert!((color.s - 0.3).abs() < f32::EPSILON);
        assert!((color.l - 0.5).abs() < f32::EPSILON);
        assert!((color.a - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn transition_points_interpolate_pixel_offsets() {
        let from = Point {
            x: px(-12.0),
            y: px(8.0),
        };
        let to = Point {
            x: px(0.0),
            y: px(0.0),
        };
        let point = transition_value(from, to, 0.25);

        assert_eq!(point.x, px(-9.0));
        assert_eq!(point.y, px(6.0));
    }

    #[test]
    fn transitions_can_be_delayed() {
        let transition = Transition::new(Duration::from_millis(120))
            .with_delay(Duration::from_millis(40))
            .into_animation();

        assert_eq!(transition.duration, Duration::from_millis(120));
        assert_eq!(transition.delay, Duration::from_millis(40));
    }
}
