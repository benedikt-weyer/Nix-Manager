//! All animation lives here, built on `mina`.
//!
//! mina is GUI-agnostic: it only animates plain structs. `Fx` owns a clock
//! that the app advances from iced's frame subscription, evaluates the
//! timelines against it, and exposes the current values to the views. It also
//! knows whether anything is still moving, so the app can stop requesting
//! frames (and idle at zero CPU) once everything has settled.

use enum_map::Enum;
use mina::prelude::*;

/// Slots for the one-shot fade/slide-in of each section.
#[derive(Clone, Copy, Debug)]
pub enum Slot {
    Header = 0,
    Overview = 1,
    Actions = 2,
    Activity = 3,
}

const SLOTS: usize = 4;
pub const ACTIONS: usize = 5;

/// Distance (px) a section slides while fading in.
pub const SLIDE: f32 = 14.0;

const ENTRANCE_SECONDS: f32 = 0.55;
const STAGGER_SECONDS: f32 = 0.08;
const HOVER_SETTLE_SECONDS: f32 = 0.35;
const BAR_SECONDS: f32 = 0.5;

#[derive(Animate, Clone, Debug, Default, PartialEq)]
pub struct Entrance {
    pub opacity: f32,
    pub offset: f32,
}

#[derive(Animate, Clone, Debug, Default, PartialEq)]
pub struct Hover {
    pub glow: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Enum)]
pub enum HoverState {
    #[default]
    Idle,
    Over,
}

#[derive(Animate, Clone, Debug, Default, PartialEq)]
pub struct Bar {
    pub value: f32,
}

#[derive(Animate, Clone, Debug, Default, PartialEq)]
pub struct Pulse {
    pub strength: f32,
}

#[derive(Animate, Clone, Debug, Default, PartialEq)]
pub struct Sweep {
    pub position: f32,
}

#[derive(Animate, Clone, Debug, Default, PartialEq)]
pub struct Dot {
    pub alpha: f32,
}

struct Reveal {
    timeline: EntranceTimeline,
    start: f32,
    end: f32,
}

pub struct Fx {
    clock: f32,
    reveals: Vec<Reveal>,
    hovers: Vec<EnumStateAnimator<HoverState, HoverTimeline>>,
    hover_settle: [f32; ACTIONS],
    bar_timeline: BarTimeline,
    bar_start: f32,
    bar_value: f32,
    pulse: PulseTimeline,
    sweep: SweepTimeline,
    dot: DotTimeline,
    looping: bool,
}

impl Fx {
    pub fn new() -> Self {
        let mut fx = Self {
            clock: 0.0,
            reveals: (0..SLOTS).map(|_| Self::reveal(0.0, 0.0)).collect(),
            hovers: (0..ACTIONS).map(|_| Self::hover_animator()).collect(),
            hover_settle: [0.0; ACTIONS],
            bar_timeline: Self::bar_timeline(0.0, 0.0),
            bar_start: 0.0,
            bar_value: 0.0,
            pulse: timeline!(Pulse 1.5s infinite reverse Easing::InOutSine
                from { strength: 0.25 }
                to { strength: 1.0 }),
            sweep: timeline!(Sweep 1.6s infinite Easing::InOutCubic
                from { position: 0.0 }
                to { position: 1.0 }),
            dot: timeline!(Dot 1.2s infinite Easing::InOutSine
                from { alpha: 0.2 }
                30% { alpha: 1.0 }
                60% { alpha: 0.2 }
                to { alpha: 0.2 }),
            looping: false,
        };

        // Initial page load: sections arrive one after another.
        for slot in [Slot::Header, Slot::Overview, Slot::Actions] {
            fx.restart(slot, slot as usize as f32 * STAGGER_SECONDS);
        }
        // Not shown until a run exists.
        fx.reveals[Slot::Activity as usize] = Self::reveal(0.0, 0.0);
        fx
    }

    fn reveal(start: f32, delay: f32) -> Reveal {
        let timeline = Entrance::timeline()
            .duration_seconds(ENTRANCE_SECONDS)
            .delay_seconds(delay)
            .default_easing(Easing::OutCubic)
            .keyframe(Entrance::keyframe(0.0).opacity(0.0).offset(SLIDE))
            .keyframe(Entrance::keyframe(1.0).opacity(1.0).offset(0.0))
            .build();
        Reveal { timeline, start, end: start + delay + ENTRANCE_SECONDS }
    }

    fn hover_animator() -> EnumStateAnimator<HoverState, HoverTimeline> {
        animator!(Hover {
            default(HoverState::Idle, { glow: 0.0 }),
            HoverState::Idle => 0.3s Easing::OutCubic to default,
            HoverState::Over => 0.18s Easing::OutCubic to { glow: 1.0 }
        })
    }

    fn bar_timeline(from: f32, to: f32) -> BarTimeline {
        Bar::timeline()
            .duration_seconds(BAR_SECONDS)
            .default_easing(Easing::OutCubic)
            .keyframe(Bar::keyframe(0.0).value(from))
            .keyframe(Bar::keyframe(1.0).value(to))
            .build()
    }

    /// Plays the entrance of `slot` again, starting now.
    pub fn restart(&mut self, slot: Slot, delay: f32) {
        self.reveals[slot as usize] = Self::reveal(self.clock, delay);
    }

    /// Whether the continuous animations (pulse, sweep, dots) should run.
    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    pub fn set_hover(&mut self, index: usize, over: bool) {
        self.hovers[index].set_state(if over { &HoverState::Over } else { &HoverState::Idle });
        self.hover_settle[index] = HOVER_SETTLE_SECONDS;
    }

    /// Eases the progress bar to `target` (0..1) from wherever it currently is.
    pub fn set_progress(&mut self, target: f32) {
        self.bar_timeline = Self::bar_timeline(self.bar_value, target.clamp(0.0, 1.0));
        self.bar_start = self.clock;
    }

    /// Snaps the bar without animating, for the start of a run.
    pub fn reset_progress(&mut self) {
        self.bar_timeline = Self::bar_timeline(0.0, 0.0);
        self.bar_value = 0.0;
        self.bar_start = self.clock;
    }

    pub fn tick(&mut self, dt: f32) {
        self.clock += dt;
        for (hover, settle) in self.hovers.iter_mut().zip(self.hover_settle.iter_mut()) {
            hover.advance(dt);
            *settle = (*settle - dt).max(0.0);
        }
        let mut bar = Bar { value: self.bar_value };
        self.bar_timeline.update(&mut bar, self.clock - self.bar_start);
        self.bar_value = bar.value;
    }

    /// True while anything still needs frames.
    pub fn animating(&self) -> bool {
        self.looping
            || self.reveals.iter().any(|r| self.clock < r.end)
            || self.hover_settle.iter().any(|s| *s > 0.0)
            || self.clock - self.bar_start < BAR_SECONDS
    }

    pub fn entrance(&self, slot: Slot) -> Entrance {
        let reveal = &self.reveals[slot as usize];
        let mut value = Entrance { opacity: 0.0, offset: SLIDE };
        reveal.timeline.update(&mut value, (self.clock - reveal.start).max(0.0));
        value
    }

    pub fn hover(&self, index: usize) -> f32 {
        self.hovers[index].current_values().glow
    }

    pub fn progress(&self) -> f32 {
        self.bar_value
    }

    /// 0.25..1, breathing.
    pub fn pulse(&self) -> f32 {
        let mut value = Pulse { strength: 0.25 };
        self.pulse.update(&mut value, self.clock);
        value.strength
    }

    /// 0..1, repeating; drives the indeterminate bar.
    pub fn sweep(&self) -> f32 {
        let mut value = Sweep { position: 0.0 };
        self.sweep.update(&mut value, self.clock);
        value.position
    }

    /// Alpha of the `index`th dot of the three-dot "working" indicator; each
    /// trails the previous by a fixed delay so a wave runs across them.
    pub fn dot(&self, index: usize) -> f32 {
        let mut value = Dot { alpha: 0.2 };
        self.dot.update(&mut value, (self.clock - 0.16 * index as f32).max(0.0));
        value.alpha
    }
}
