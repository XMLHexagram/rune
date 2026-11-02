//! The decisions of a song with a chart, stepped by song time.
//!
//! With a chart the scene's [`Decider`] is not stepped by the frames the screen draws but by the
//! song: every frame takes the steps of [`STEP`] whose song time has come (none at a high frame
//! rate, a few after a slow frame), each heard from the chart alone with the song's seed, as
//! `crate::plan` plays the song ahead of time. The cuts, looks and moments then come at the same
//! song times, and are the same, whatever the frame rate, and whenever the scene was opened.
//!
//! Where the song time jumps (the scene opens on a song under way, a seek) the decider is put
//! where it would be by playing the song from its start, silently: from the latest snapshot
//! before that time, which are kept every ten seconds of song from such replays and from playing
//! on untouched, else from the start. What it skipped is not played: the screen jumps to the
//! shot on, without a transition or an effect.
//!
//! A chart that comes while the scene plays (the analysis of the song finished) does not jump:
//! the song's decider is stepped next to the one on screen until it cuts, and the screen takes it
//! over on that cut.

use std::sync::Arc;

use super::CutInfo;
use super::decider::{Decider, Decision, STEP, Settings};
use super::inputs::Levels;
use super::lens::LensFrame;
use super::manifest::Manifest;
use crate::chart::Chart;
use crate::inspect::LabEvent;
use crate::rhythm::Rhythm;

/// A song's whole chart and the seed its decisions are made with (see `crate::plan::seed_of`).
#[derive(Clone)]
pub struct SongChart {
    pub chart: Arc<Chart>,
    pub seed: u32,
}

impl SongChart {
    fn is(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.chart, &other.chart) && self.seed == other.seed
    }
}

/// A snapshot of the decider is kept every this many steps: ten seconds of song.
const SNAPSHOT_EVERY: u32 = 600;
/// A song time up to this many steps behind the steps taken is waited for rather than taken for a
/// seek: the song clock closes a small gap to the player by pulling back a little.
const BEHIND: u32 = 6;
/// A song time up to this many steps ahead (a second) is stepped through, as after a slow frame;
/// further ahead is a seek.
const AHEAD: u32 = 60;

/// What a frame decided, stepped by song time.
pub struct Stepped {
    /// What the steps of the frame decided, the last of each kind: a cut, a look change, a
    /// moment, a glitch or mosaic. The lens is that of the last step taken, also in a frame that
    /// takes none.
    pub decision: Decision,
    /// The event log of the steps, each at its own song time.
    pub events: Vec<LabEvent>,
    /// The decider was put where the song is without playing what it skipped: the screen jumps to
    /// the shot on.
    pub jumped: bool,
    /// The decider is another one than in the last frame (a jump, or the song's own decider taking
    /// over on its cut): what it counted is not to be compared with what the last one did.
    pub replaced: bool,
    /// Song seconds since the time of the last step taken, by which what runs with time (a shot's
    /// age) is ahead of what the decider says.
    pub since_step: f64,
}

/// Steps a scene's decider by the song (see the module's doc).
#[derive(Default)]
pub struct Stepper {
    /// The song the decider on screen is stepped through, while it is.
    on: Option<SongChart>,
    /// How many steps it took: it decided the frames at song times 0, [`STEP`], … up to
    /// `steps - 1` steps.
    steps: u32,
    /// The decider before step n of the song, for the n kept, in order.
    snapshots: Vec<(u32, Decider)>,
    /// What the snapshots are of: the song, what a lab or the user set on the decider, and the
    /// version of the runtime's decisions.
    of: Option<(SongChart, Settings, u64)>,
    /// Whether the decider on screen is as the song alone made it, which a lab's command breaks
    /// (see [`Stepper::taint`]); only then are its snapshots kept.
    pure: bool,
    /// The song's own decider while another is on screen, with the steps it took.
    shadow: Option<(SongChart, Decider, u32)>,
    /// What the post pass was told by the last step.
    lens: LensFrame,
}

/// How many steps are due at song time `time`: those at song times up to it.
fn steps_until(time: f64) -> u32 {
    (time.max(0.0) / STEP).floor() as u32 + 1
}

/// The live audio a step hears: nothing, since the chart decides (as in a plan of the song).
fn unheard() -> (Rhythm, Levels) {
    (Rhythm::default(), Levels::default())
}

/// Takes what `step` decided into `frame`, where the last of each kind stands.
fn merge(frame: &mut Decision, step: Decision) {
    frame.cut = step.cut.or(frame.cut);
    frame.shot = step.shot;
    frame.look = step.look.or(frame.look);
    frame.moment = step.moment.or(frame.moment);
    frame.lens = step.lens;
    frame.overlay = step.overlay.or(frame.overlay);
}

impl Stepper {
    /// The frame at song time `time` of `song`, for `decider`, the decider of a scene of
    /// `manifest`. `forced` is a cut a lab made, taken by the first step of the frame; `first`
    /// tells the scene's first frame; `runtime` is the version of the runtime's decisions the
    /// decider reads. None while a chart that came during the song waits for its decider's cut:
    /// the caller decides that frame itself.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &mut self,
        decider: &mut Decider,
        manifest: &Manifest,
        song: &SongChart,
        time: f64,
        forced: &mut Option<CutInfo>,
        quality: f32,
        first: bool,
        runtime: u64,
    ) -> Option<Stepped> {
        self.check_snapshots(decider, song, runtime);
        let target = steps_until(time);
        if !self.on.as_ref().is_some_and(|on| on.is(song)) {
            if first {
                // Nothing on screen to keep: the scene opens where the song is.
                self.shadow = None;
                *forced = None;
                self.restore(decider, manifest, song, target, quality);
                return Some(self.jump(decider, time));
            }
            return self.follow(decider, manifest, song, target, time, forced, quality);
        }
        if target + BEHIND < self.steps || target > self.steps + AHEAD {
            // A cut a lab made is of the decider replaced.
            *forced = None;
            self.restore(decider, manifest, song, target, quality);
            return Some(self.jump(decider, time));
        }
        let (decision, events) = self.run(decider, song, target, forced, quality);
        Some(Stepped {
            decision,
            events,
            jumped: false,
            replaced: false,
            since_step: self.since_step(time),
        })
    }

    /// The song has no chart (or a lab holds the music without one): the caller decides frame by
    /// frame. The snapshots stay, for the chart that may come back.
    pub fn off(&mut self) {
        self.on = None;
        self.shadow = None;
    }

    /// A lab changed the decider on screen by hand (a cut, an effect, a hold, an override): it is
    /// no longer what the song alone makes, and no snapshot is taken from it.
    pub fn taint(&mut self) {
        self.pure = false;
    }

    /// Drops the snapshots when they are of another song, or of the decider set otherwise.
    fn check_snapshots(&mut self, decider: &Decider, song: &SongChart, runtime: u64) {
        let settings = decider.settings();
        let same = self
            .of
            .as_ref()
            .is_some_and(|(of, of_settings, of_runtime)| {
                of.is(song) && *of_settings == settings && *of_runtime == runtime
            });
        if !same {
            self.snapshots.clear();
            self.of = Some((song.clone(), settings, runtime));
            self.pure = false;
            self.shadow = None;
        }
    }

    /// Takes the steps of `song` due up to `target` (not counting) with the decider on screen.
    fn run(
        &mut self,
        decider: &mut Decider,
        song: &SongChart,
        target: u32,
        forced: &mut Option<CutInfo>,
        quality: f32,
    ) -> (Decision, Vec<LabEvent>) {
        let mut frame = self.standing(decider);
        let mut events = Vec::new();
        while self.steps < target {
            if self.pure {
                self.keep(self.steps, decider);
            }
            let step = self.steps;
            let decision = decider.step_at(
                &song.chart,
                step,
                song.seed,
                unheard(),
                forced.take(),
                quality,
            );
            events.extend(decider.events(f64::from(step) * STEP, &decision, false));
            merge(&mut frame, decision);
            self.steps += 1;
        }
        self.lens = frame.lens;
        (frame, events)
    }

    /// The song's own decider, stepped next to the one on screen until it cuts; then it is put on
    /// screen and the frame reports its cut.
    #[allow(clippy::too_many_arguments)]
    fn follow(
        &mut self,
        decider: &mut Decider,
        manifest: &Manifest,
        song: &SongChart,
        target: u32,
        time: f64,
        forced: &mut Option<CutInfo>,
        quality: f32,
    ) -> Option<Stepped> {
        let (mut shadow, mut steps) = match self.shadow.take() {
            Some((of, shadow, steps))
                if of.is(song) && target + BEHIND >= steps && target <= steps + AHEAD =>
            {
                (shadow, steps)
            }
            // Started now, or the song time jumped: from where the song is.
            _ => (
                self.replay(decider, manifest, song, target, quality),
                target,
            ),
        };
        while steps < target {
            let decision = shadow.step_at(&song.chart, steps, song.seed, unheard(), None, quality);
            let step = steps;
            steps += 1;
            if decision.cut.is_none() {
                continue;
            }
            // The song's decider cut: it takes over with this cut, and the screen hears of the
            // look it is on.
            *decider = shadow;
            self.on = Some(song.clone());
            self.steps = steps;
            self.pure = true;
            let at = f64::from(step) * STEP;
            let mut events = decider.events(at, &decision, false);
            let mut frame = decision;
            if frame.look.is_none()
                && let Some(look) = decider.look_on()
            {
                frame.look = Some(look);
                let told = Decision {
                    cut: None,
                    look: Some(look),
                    moment: None,
                    overlay: None,
                    ..frame.clone()
                };
                events.extend(decider.events(at, &told, false));
            }
            let (rest, more) = self.run(decider, song, target, forced, quality);
            merge(&mut frame, rest);
            events.extend(more);
            self.lens = frame.lens;
            return Some(Stepped {
                decision: frame,
                events,
                jumped: false,
                replaced: true,
                since_step: self.since_step(time),
            });
        }
        self.shadow = Some((song.clone(), shadow, steps));
        None
    }

    /// Puts `decider` where `song` is before step `target`, silently (see [`Stepper::replay`]).
    fn restore(
        &mut self,
        decider: &mut Decider,
        manifest: &Manifest,
        song: &SongChart,
        target: u32,
        quality: f32,
    ) {
        *decider = self.replay(decider, manifest, song, target, quality);
        self.on = Some(song.clone());
        self.steps = target;
        self.pure = true;
    }

    /// A decider set like `decider` that played `song` up to step `target` (not counting), from the
    /// latest snapshot before it or else from the start, keeping snapshots on the way. Nothing it
    /// decides on the way is reported.
    fn replay(
        &mut self,
        decider: &Decider,
        manifest: &Manifest,
        song: &SongChart,
        target: u32,
        quality: f32,
    ) -> Decider {
        let (mut steps, mut replayed) =
            match self.snapshots.iter().rev().find(|(s, _)| *s <= target) {
                Some((steps, snapshot)) => (*steps, snapshot.clone()),
                None => (0, decider.fresh(manifest)),
            };
        while steps < target {
            self.keep(steps, &replayed);
            self.lens = replayed
                .step_at(&song.chart, steps, song.seed, unheard(), None, quality)
                .lens;
            steps += 1;
        }
        replayed
    }

    /// Keeps a snapshot of `decider` before step `steps`, when one is due there and not kept yet.
    fn keep(&mut self, steps: u32, decider: &Decider) {
        if !steps.is_multiple_of(SNAPSHOT_EVERY) {
            return;
        }
        if let Err(at) = self.snapshots.binary_search_by_key(&steps, |(s, _)| *s) {
            self.snapshots.insert(at, (steps, decider.clone()));
        }
    }

    /// The frame of a jump: the shot and the look the decider is on, as the scene's first frame
    /// tells them, and nothing else.
    fn jump(&self, decider: &Decider, time: f64) -> Stepped {
        let decision = Decision {
            look: decider.look_on(),
            ..self.standing(decider)
        };
        Stepped {
            events: decider.events(time, &decision, true),
            decision,
            jumped: true,
            replaced: true,
            since_step: self.since_step(time),
        }
    }

    /// A frame in which nothing is decided.
    fn standing(&self, decider: &Decider) -> Decision {
        Decision {
            cut: None,
            shot: decider.shot_index(),
            look: None,
            moment: None,
            lens: self.lens,
            overlay: None,
        }
    }

    fn since_step(&self, time: f64) -> f64 {
        let last = f64::from(self.steps.saturating_sub(1)) * STEP;
        (time - last).max(0.0)
    }

    #[cfg(test)]
    fn snapshot_steps(&self) -> Vec<u32> {
        self.snapshots.iter().map(|(s, _)| *s).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::LabEventKind;
    use crate::plan::{self, SongPlan};
    use crate::scene::fixtures;

    /// A quiet section, a high one the director bursts into, and a quieter one.
    const SECTIONS: [(f64, f64, f64); 3] =
        [(0.0, 30.0, 0.2), (30.0, 70.0, 0.9), (70.0, 100.0, 0.3)];

    fn song() -> SongChart {
        SongChart {
            chart: Arc::new(fixtures::chart(&SECTIONS)),
            seed: plan::seed_of("/music/a.flac"),
        }
    }

    fn planned(song: &SongChart) -> SongPlan {
        plan::plan_song(&song.chart, fixtures::decider(), song.seed)
    }

    /// An event as the tests compare them: its time to the step, and what it was.
    fn key(event: &LabEvent) -> (u32, String) {
        let what = match &event.kind {
            LabEventKind::Cut {
                shot, fill, strong, ..
            } => format!("cut {shot} {fill} {strong}"),
            LabEventKind::Look { look } => format!("look {look}"),
            LabEventKind::Moment { strength, effect } => format!("moment {strength} {effect:?}"),
            LabEventKind::Lab { .. } => String::new(),
        };
        ((event.time / STEP).round() as u32, what)
    }

    /// The plan's events after song time `from` up to `to`, as [`key`] gives them.
    fn plan_keys(plan: &SongPlan, from: f64, to: f64) -> Vec<(u32, String)> {
        let mut events: Vec<LabEvent> = Vec::new();
        let at = |t: f64| t > from && t <= to;
        for c in plan.cuts.iter().filter(|c| at(c.time) && !c.first) {
            events.push(LabEvent {
                time: c.time,
                kind: LabEventKind::Cut {
                    shot: c.shot.clone(),
                    size: c.size.clone(),
                    fill: c.fill,
                    strong: c.strong,
                    hard: c.hard,
                    dress: c.dress.clone(),
                    first: false,
                },
            });
        }
        for l in plan.looks.iter().filter(|l| at(l.time) && l.time > 0.0) {
            events.push(LabEvent {
                time: l.time,
                kind: LabEventKind::Look {
                    look: l.look.clone(),
                },
            });
        }
        for m in plan.moments.iter().filter(|m| at(m.time)) {
            events.push(LabEvent {
                time: m.time,
                kind: LabEventKind::Moment {
                    strength: m.strength.clone(),
                    effect: m.effect.clone(),
                },
            });
        }
        let mut keys: Vec<_> = events.iter().map(key).collect();
        keys.sort();
        keys
    }

    /// Plays `song` on screen through these song times, one frame each, the first being the
    /// scene's first, and returns the events of the frames that did not jump, with the song times
    /// of those that did.
    fn play(
        stepper: &mut Stepper,
        decider: &mut Decider,
        song: &SongChart,
        times: &[f64],
    ) -> (Vec<(u32, String)>, Vec<f64>) {
        let manifest = fixtures::manifest();
        let mut keys = Vec::new();
        let mut jumps = Vec::new();
        for (i, &t) in times.iter().enumerate() {
            let stepped = stepper
                .frame(decider, &manifest, song, t, &mut None, 1.0, i == 0, 0)
                .expect("stepped");
            if stepped.jumped {
                jumps.push(t);
                continue;
            }
            keys.extend(stepped.events.iter().map(key));
        }
        keys.sort();
        (keys, jumps)
    }

    /// Song times from `from` to `to` at a frame rate that wanders between 30 and 200 fps.
    fn jittery(from: f64, to: f64) -> Vec<f64> {
        let mut times = Vec::new();
        let mut t = from;
        let mut i = 0u32;
        while t < to {
            times.push(t);
            let wander = (f64::from(i) * 12.9898).sin().abs();
            t += 1.0 / 200.0 + wander * (1.0 / 30.0 - 1.0 / 200.0);
            i += 1;
        }
        times
    }

    #[test]
    fn the_screen_decides_what_the_plan_says_at_any_frame_rate() {
        let song = song();
        let plan = planned(&song);
        let mut decider = fixtures::decider();
        let mut stepper = Stepper::default();
        let times = jittery(0.0, 100.0);
        let (keys, jumps) = play(&mut stepper, &mut decider, &song, &times);
        assert_eq!(jumps, [0.0]);
        assert!(keys.len() > 10, "{keys:?}");
        assert_eq!(keys, plan_keys(&plan, 0.0, *times.last().unwrap()));
    }

    #[test]
    fn a_scene_opened_on_a_song_under_way_goes_on_as_if_it_had_played_it_all() {
        let song = song();
        let plan = planned(&song);
        let mut decider = fixtures::decider();
        let mut stepper = Stepper::default();
        let times = jittery(47.3, 100.0);
        let (keys, jumps) = play(&mut stepper, &mut decider, &song, &times);
        assert_eq!(jumps, [47.3]);
        assert_eq!(keys, plan_keys(&plan, 47.3, *times.last().unwrap()));
        // Snapshots are kept every ten seconds of song: from the replay up to the opening, then
        // from playing on.
        let every_ten: Vec<u32> = (0..10).map(|i| i * 600).collect();
        assert_eq!(stepper.snapshot_steps(), every_ten);
    }

    #[test]
    fn a_seek_jumps_and_the_song_goes_on_from_there_as_planned() {
        let song = song();
        let plan = planned(&song);
        let mut decider = fixtures::decider();
        let mut stepper = Stepper::default();
        let segments = [
            jittery(0.0, 60.0),
            jittery(21.0, 40.0),
            jittery(80.0, 100.0),
        ];
        let times = segments.concat();
        let (keys, jumps) = play(&mut stepper, &mut decider, &song, &times);
        assert_eq!(jumps, [0.0, 21.0, 80.0]);
        let mut expected: Vec<_> = segments
            .iter()
            .flat_map(|times| plan_keys(&plan, times[0], *times.last().unwrap()))
            .collect();
        expected.sort();
        assert_eq!(keys, expected);
    }

    #[test]
    fn a_song_clock_that_pulls_back_a_little_is_waited_for() {
        let song = song();
        let manifest = fixtures::manifest();
        let mut decider = fixtures::decider();
        let mut stepper = Stepper::default();
        let mut frame = |t: f64, first: bool| {
            stepper
                .frame(&mut decider, &manifest, &song, t, &mut None, 1.0, first, 0)
                .unwrap()
        };
        frame(10.0, true);
        frame(10.5, false);
        let back = frame(10.45, false);
        assert!(!back.jumped && back.decision.cut.is_none() && back.events.is_empty());
        assert!(!frame(10.52, false).jumped);
        assert!(frame(10.0, false).jumped);
        assert!(frame(12.0, false).jumped);
    }

    #[test]
    fn a_chart_that_comes_during_the_song_takes_over_on_its_own_cut() {
        let song = song();
        let plan = planned(&song);
        let manifest = fixtures::manifest();
        let mut decider = fixtures::decider();
        let mut stepper = Stepper::default();
        // The scene has been on for a while, deciding by itself.
        let mut times = jittery(5.0, 100.0).into_iter();
        let first = times.next().unwrap();
        assert!(
            stepper
                .frame(
                    &mut decider,
                    &manifest,
                    &song,
                    first,
                    &mut None,
                    1.0,
                    false,
                    0
                )
                .is_none()
        );
        let next_cut = plan
            .cuts
            .iter()
            .find(|c| c.time > first)
            .expect("a cut")
            .time;
        let mut took_over = None;
        for t in times.by_ref() {
            if let Some(stepped) =
                stepper.frame(&mut decider, &manifest, &song, t, &mut None, 1.0, false, 0)
            {
                assert!(stepped.replaced && !stepped.jumped);
                assert!(stepped.decision.cut.is_some());
                took_over = Some((t, stepped.events[0].time));
                break;
            }
        }
        let (at, cut) = took_over.expect("took over");
        assert_eq!(cut, next_cut);
        // From then on it is the song's decider.
        let later: Vec<f64> = times.collect();
        let (keys, jumps) = play_on(&mut stepper, &mut decider, &song, &later);
        assert!(jumps.is_empty());
        assert_eq!(keys, plan_keys(&plan, at, *later.last().unwrap()));
    }

    /// [`play`] on a scene that already shows the song.
    fn play_on(
        stepper: &mut Stepper,
        decider: &mut Decider,
        song: &SongChart,
        times: &[f64],
    ) -> (Vec<(u32, String)>, Vec<f64>) {
        let manifest = fixtures::manifest();
        let mut keys = Vec::new();
        let mut jumps = Vec::new();
        for &t in times {
            let stepped = stepper
                .frame(decider, &manifest, song, t, &mut None, 1.0, false, 0)
                .expect("stepped");
            if stepped.jumped {
                jumps.push(t);
            }
            keys.extend(stepped.events.iter().map(key));
        }
        keys.sort();
        (keys, jumps)
    }

    #[test]
    fn a_change_of_what_the_decider_is_set_to_drops_the_snapshots() {
        let song = song();
        let manifest = fixtures::manifest();
        let mut decider = fixtures::decider();
        let mut stepper = Stepper::default();
        stepper.frame(
            &mut decider,
            &manifest,
            &song,
            25.0,
            &mut None,
            1.0,
            true,
            0,
        );
        assert_eq!(stepper.snapshot_steps(), [0, 600, 1200]);
        decider
            .director_mut()
            .unwrap()
            .set_pace_override(Some(crate::scene::Pace::Calm));
        stepper.frame(
            &mut decider,
            &manifest,
            &song,
            25.02,
            &mut None,
            1.0,
            false,
            0,
        );
        assert!(stepper.snapshot_steps().is_empty());
        // So does a new version of the runtime's decisions.
        stepper.frame(
            &mut decider,
            &manifest,
            &song,
            5.0,
            &mut None,
            1.0,
            false,
            0,
        );
        assert_eq!(stepper.snapshot_steps(), [0]);
        stepper.frame(
            &mut decider,
            &manifest,
            &song,
            5.02,
            &mut None,
            1.0,
            false,
            1,
        );
        assert!(stepper.snapshot_steps().is_empty());
    }

    #[test]
    fn a_decider_a_lab_changed_by_hand_keeps_no_snapshot() {
        let song = song();
        let manifest = fixtures::manifest();
        let mut decider = fixtures::decider();
        let mut stepper = Stepper::default();
        stepper.frame(&mut decider, &manifest, &song, 1.0, &mut None, 1.0, true, 0);
        stepper.taint();
        for t in jittery(1.0, 25.0) {
            stepper.frame(&mut decider, &manifest, &song, t, &mut None, 1.0, false, 0);
        }
        assert_eq!(stepper.snapshot_steps(), [0]);
    }
}
