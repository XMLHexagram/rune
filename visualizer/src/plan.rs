//! A song played through ahead of time: what the camera, the looks and the moments of the music
//! will do in it, for a scene lab to show before it happens.
//!
//! The plan is made by stepping a fresh [`Decider`] (see `scene::decider`) over the chart from
//! the start to the end of the song, one 60th of a second at a time, through the same step the
//! scene on screen takes. Everything it reads is the chart and the seed, so the same song and
//! settings plan the same thing each time; the live audio, which the chart replaces wherever it
//! has a value, is left out.

use crate::chart::{Chart, trusted_rhythm};
use crate::inspect::LabEventKind;
use crate::rhythm::Rhythm;
use crate::scene::Decider;
use crate::scene::Levels;

/// The step of the plan, in seconds: a frame at 60 fps.
pub const STEP: f64 = 1.0 / 60.0;

/// The quality the plan's lens is stepped with: the flare gives way only to a low quality, and
/// that changes what is drawn, not what is decided.
const QUALITY: f32 = 1.0;

/// One section of the song.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PlanSection {
    /// Seconds.
    pub start: f64,
    pub end: f64,
    pub label: String,
    pub energy: f32,
    /// How lively the director takes it to be, by the runtime's decisions: "quiet", "normal" or
    /// "high".
    pub tier: String,
}

/// A cut of the camera.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PlanCut {
    /// Song seconds.
    pub time: f64,
    pub shot: String,
    /// "wide", "medium", "close", "detail" or "top".
    pub size: String,
    /// 0 for a normal cut, else the position in a burst (1 to 4).
    pub fill: u32,
    /// The cut lands on a strong transition.
    pub strong: bool,
    /// The view jumps, rather than glides.
    pub hard: bool,
    /// The transition that dresses it, by name.
    pub dress: Option<String>,
    /// The scene's first shot, which it begins with.
    pub first: bool,
}

/// A change of look: the first one is the look the scene opens in.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PlanLook {
    pub time: f64,
    pub look: String,
}

/// A moment of the music that plays an effect.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PlanMoment {
    pub time: f64,
    /// "medium" or "strong".
    pub strength: String,
    /// What it plays, by name: a transition that dresses a cut, or a lens effect; none if the
    /// scene lists nothing that suits the moment.
    pub effect: Option<String>,
}

/// What a scene does in a song, as played through ahead of time (see the module's doc).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct SongPlan {
    /// Which plan this is among those made, so that a reader sees a new one.
    pub version: u64,
    /// The song's file, and the scene (its name) the plan is of.
    pub path: String,
    pub scene: String,
    /// Seconds.
    pub length: f64,
    pub sections: Vec<PlanSection>,
    pub cuts: Vec<PlanCut>,
    pub looks: Vec<PlanLook>,
    pub moments: Vec<PlanMoment>,
}

/// The seed a song is planned with: a hash of its file (FNV-1a), so that it is the same each time
/// and does not depend on the run. The looks pick the scene's first look, and so which later
/// section starts change it, from the seed.
pub fn seed_of(path: &str) -> u32 {
    path.bytes().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// Play `chart` through `decider` (which must not have heard anything yet) from the start to the
/// end of the song with this `seed`. The file and the scene are left for the caller to say.
pub fn plan_song(chart: &Chart, decider: Decider, seed: u32) -> SongPlan {
    play(chart, decider, seed, |_| {
        (Rhythm::default(), Levels::default())
    })
}

/// [`plan_song`] with a live tracker and levels at each song time: what the chart leaves of the
/// live audio, which decides nothing in a song with a chart (a test says so).
fn play(
    chart: &Chart,
    mut decider: Decider,
    seed: u32,
    live: impl Fn(f64) -> (Rhythm, Levels),
) -> SongPlan {
    let length = chart.length();
    let levels: Vec<f32> = chart
        .at(0.0)
        .song
        .map_or_else(Vec::new, |song| song.iter().map(|s| s.level).collect());
    let mut plan = SongPlan {
        length,
        sections: chart
            .sections()
            .into_iter()
            .zip(&levels)
            .map(|(section, &level)| PlanSection {
                start: section.start,
                end: section.end,
                label: section.label,
                energy: section.energy,
                tier: decider.tier_at(level).to_string(),
            })
            .collect(),
        ..SongPlan::default()
    };
    for frame in 0..(length / STEP).ceil() as u32 {
        let time = f64::from(frame) * STEP;
        let state = chart.at(time);
        let (tracker, levels) = live(time);
        let rhythm = trusted_rhythm(tracker, Some(&state));
        let music = decider.hear(&rhythm, Some(&state), levels, (time, true), STEP);
        let decision = decider.step(STEP as f32, &music, Some(&state), seed, None, QUALITY);
        for event in decider.events(time, &decision, frame == 0) {
            match event.kind {
                LabEventKind::Cut {
                    shot,
                    size,
                    fill,
                    strong,
                    hard,
                    dress,
                    first,
                } => plan.cuts.push(PlanCut {
                    time: event.time,
                    shot,
                    size,
                    fill,
                    strong,
                    hard,
                    dress,
                    first,
                }),
                LabEventKind::Look { look } => plan.looks.push(PlanLook {
                    time: event.time,
                    look,
                }),
                LabEventKind::Moment { strength, effect } => plan.moments.push(PlanMoment {
                    time: event.time,
                    strength,
                    effect,
                }),
                LabEventKind::Lab { .. } => {}
            }
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::fixtures;

    /// A song of a quiet section and a high one, which the director leads to with a burst.
    const SECTIONS: [(f64, f64, f64); 3] =
        [(0.0, 30.0, 0.2), (30.0, 70.0, 0.9), (70.0, 100.0, 0.3)];

    /// Noise in place of the live audio: kicks, energy and bands that change every frame.
    fn noisy(t: f64) -> (Rhythm, Levels) {
        let h = |k: f64| ((t * 1000.0 + k).sin() * 43758.5453).fract().abs();
        (
            Rhythm {
                bpm: 97.0,
                beat: t * 1.6,
                confidence: 0.9,
                kick: h(1.0),
                snare: h(2.0),
                hat: h(3.0),
                energy: 2.0 * h(4.0),
            },
            Levels {
                bass: (3.0 * h(5.0)) as f32,
                mids: (3.0 * h(6.0)) as f32,
                treble: (3.0 * h(7.0)) as f32,
                feel: 6.0,
                song_feel: 6.0,
            },
        )
    }

    #[test]
    fn the_seed_follows_the_song() {
        assert_eq!(seed_of(""), 0x811c_9dc5);
        assert_eq!(seed_of("a"), 0xe40c_292c);
        assert_ne!(seed_of("/music/a.flac"), seed_of("/music/b.flac"));
    }

    #[test]
    fn a_plan_is_the_same_each_time_whatever_the_live_audio_says() {
        let chart = fixtures::chart(&SECTIONS);
        let seed = seed_of("/music/a.flac");
        let plan = plan_song(&chart, fixtures::decider(), seed);
        assert!(
            !plan.cuts.is_empty() && !plan.moments.is_empty(),
            "{plan:?}"
        );
        assert_eq!(plan, plan_song(&chart, fixtures::decider(), seed));
        assert_eq!(plan, play(&chart, fixtures::decider(), seed, noisy));
    }

    #[test]
    fn the_plan_describes_the_song() {
        let chart = fixtures::chart(&SECTIONS);
        let plan = plan_song(&chart, fixtures::decider(), 1);
        assert_eq!(plan.length, 100.0);
        let tiers: Vec<_> = plan.sections.iter().map(|s| s.tier.as_str()).collect();
        assert_eq!(tiers, ["quiet", "high", "normal"]);
        assert_eq!(
            plan.sections
                .iter()
                .map(|s| s.label.as_str())
                .collect::<Vec<_>>(),
            ["A", "B", "C"]
        );
        // The scene begins on its first shot, and the looks on their first look.
        assert!(plan.cuts[0].first && plan.cuts[0].time == 0.0);
        assert!(plan.cuts.iter().skip(1).all(|c| !c.first));
        assert_eq!(plan.looks[0].time, 0.0);
    }

    #[test]
    fn a_burst_comes_before_a_lifting_section_and_a_strong_cut_follows() {
        let chart = fixtures::chart(&SECTIONS);
        let plan = plan_song(&chart, fixtures::decider(), 1);
        let fills: Vec<(u32, bool)> = plan
            .cuts
            .iter()
            .filter(|cut| cut.time < 31.0 && cut.time > 20.0)
            .map(|cut| (cut.fill, cut.strong))
            .collect();
        let burst = fills
            .iter()
            .position(|&(fill, _)| fill == 1)
            .expect("a burst");
        assert_eq!(
            fills[burst..],
            [(1, false), (2, false), (3, false), (4, false), (0, true)]
        );
        let last = plan.cuts.iter().position(|cut| cut.fill == 4).unwrap();
        assert!(plan.cuts[last].hard && plan.cuts[last + 1].hard);
        assert!(
            (30.0..30.05).contains(&plan.cuts[last + 1].time),
            "{plan:?}"
        );
    }

    /// The scene drawing frame by frame (the music's values by the step, then the decision) gives
    /// the cuts, looks and moments the plan says, and so does the event log.
    #[test]
    fn the_scene_on_screen_decides_what_the_plan_says() {
        let chart = fixtures::chart(&SECTIONS);
        let seed = seed_of("/music/a.flac");
        let plan = plan_song(&chart, fixtures::decider(), seed);
        let mut decider = fixtures::decider();
        let (mut cuts, mut looks, mut moments) = (Vec::new(), Vec::new(), Vec::new());
        for frame in 0..(plan.length / STEP).ceil() as u32 {
            let time = f64::from(frame) * STEP;
            let state = chart.at(time);
            let rhythm = trusted_rhythm(Rhythm::default(), Some(&state));
            let music = decider.hear(&rhythm, Some(&state), Levels::default(), (time, true), STEP);
            let decision = decider.step(STEP as f32, &music, Some(&state), seed, None, 0.5);
            for event in decider.events(time, &decision, frame == 0) {
                match event.kind {
                    LabEventKind::Cut {
                        shot, fill, strong, ..
                    } => {
                        cuts.push((event.time, shot, fill, strong));
                    }
                    LabEventKind::Look { look } => looks.push((event.time, look)),
                    LabEventKind::Moment { strength, effect } => {
                        moments.push((event.time, strength, effect));
                    }
                    LabEventKind::Lab { .. } => {}
                }
            }
        }
        let planned: Vec<_> = plan
            .cuts
            .iter()
            .map(|c| (c.time, c.shot.clone(), c.fill, c.strong))
            .collect();
        assert_eq!(cuts, planned);
        let planned: Vec<_> = plan
            .looks
            .iter()
            .map(|l| (l.time, l.look.clone()))
            .collect();
        assert_eq!(looks, planned);
        let planned: Vec<_> = plan
            .moments
            .iter()
            .map(|m| (m.time, m.strength.clone(), m.effect.clone()))
            .collect();
        assert_eq!(moments, planned);
    }

    #[test]
    fn a_plan_follows_what_a_lab_set() {
        let chart = fixtures::chart(&SECTIONS);
        let plain = plan_song(&chart, fixtures::decider(), 1);
        let mut lab = fixtures::decider();
        lab.director_mut()
            .unwrap()
            .set_pace_override(Some(crate::scene::Pace::Sections));
        let slow = plan_song(&chart, lab.fresh(&fixtures::manifest()), 1);
        assert!(slow.cuts.len() < plain.cuts.len(), "{slow:?} {plain:?}");
    }
}
