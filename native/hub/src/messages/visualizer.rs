use rinf::{DartSignal, RustSignal, SignalPiece};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, DartSignal)]
pub struct StartVisualizerRequest {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Serialize, Deserialize, DartSignal)]
pub struct StopVisualizerRequest {}

#[derive(Serialize, Deserialize, DartSignal)]
pub struct ResizeVisualizerRequest {
    pub width: u32,
    pub height: u32,
}

/// Directories with scenes (folders with a `scene.toml`) listed beside the built-in ones: the
/// app's folder of imported scenes. A running visualizer whose scene is gone changes to the
/// built-in scene.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct SetVisualizerPresetDirsRequest {
    pub paths: Vec<String>,
}

/// The app's accent colour as 0xAARRGGBB, for scenes that draw in it.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct SetVisualizerAccentRequest {
    pub argb: u32,
}

/// Whether the app's window can be seen. While it cannot, the visualizer stands still.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct SetVisualizerVisibleRequest {
    pub visible: bool,
}

/// Shows the scene at `path`: a scene folder, or `builtin:<name>` for a built-in scene (as the
/// scenes of [`ListVisualizerPresetsResponse`] say).
#[derive(Serialize, Deserialize, DartSignal)]
pub struct LoadVisualizerPresetRequest {
    pub path: String,
}

#[derive(Serialize, Deserialize, DartSignal)]
pub struct NextVisualizerPresetRequest {}

#[derive(Serialize, Deserialize, DartSignal)]
pub struct PreviousVisualizerPresetRequest {}

#[derive(Serialize, Deserialize, DartSignal)]
pub struct RandomVisualizerPresetRequest {}

/// How the scenes are picked (paths in the form of `VisualizerPresetEntry::path`), replacing what
/// was set before. "Next" goes through the scenes in `order`, and the visualizer begins with its
/// first one when there is no scene to resume; scenes not in it come after it. `disabled` are left out, but can
/// still be loaded by path; turning every scene off counts as turning none off.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct SetVisualizerScenesRequest {
    pub order: Vec<String>,
    pub disabled: Vec<String>,
    /// The scene on screen when the app last ran, which a visualizer that starts resumes unless
    /// another scene was shown since the app started.
    pub last: Option<String>,
}

/// Unpack the scene file `file` (a `.runescene`, see `visualizer::package`) into a folder of its
/// own in `dir`, the app's folder of imported scenes, replacing a scene of that name. The scenes
/// of `dir` are listed again.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct ImportVisualizerSceneRequest {
    pub file: String,
    pub dir: String,
}

/// `path` is the folder of the imported scene, or `error` says why the file is not one.
#[derive(Serialize, Deserialize, RustSignal)]
pub struct ImportVisualizerSceneResponse {
    pub file: String,
    pub path: Option<String>,
    pub error: Option<String>,
}

/// Delete the imported scene at `path`, which must be a folder directly in `dir`. The scenes of
/// `dir` are listed again.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct RemoveVisualizerSceneRequest {
    pub path: String,
    pub dir: String,
}

#[derive(Serialize, Deserialize, RustSignal)]
pub struct RemoveVisualizerSceneResponse {
    pub path: String,
    pub error: Option<String>,
}

/// Whether the visualizer's diagnostics (the GPU it runs on, how frames reach the screen) go to the
/// Log page; its errors always do. The app sends its setting at start and whenever it changes,
/// and the Log page's entries are written from the first one on.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct SetVisualizerLogRequest {
    pub enabled: bool,
}

/// Whether this machine can run the visualizer, asked before starting it.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct VisualizerSupportRequest {}

/// `supported`: there is a GPU (`adapter` names it). Otherwise `problem` says why not: no GPU
/// adapter, or only a software renderer, which would draw the scenes on the CPU; then only the
/// cover wall can be shown.
#[derive(Serialize, Deserialize, RustSignal)]
pub struct VisualizerSupportResponse {
    pub supported: bool,
    pub adapter: String,
    pub problem: Option<String>,
}

#[derive(Serialize, Deserialize, DartSignal)]
pub struct VisualizerStatusRequest {}

#[derive(Serialize, Deserialize, RustSignal)]
pub struct VisualizerStatusResponse {
    pub running: bool,
    pub width: u32,
    pub height: u32,
    pub target_fps: u32,
    pub measured_fps: f32,
    pub preset_name: String,
    /// Where the scene on screen is, in the form of `VisualizerPresetEntry::path`;
    /// `app:cover-wall` while the cover wall is on and nothing is drawn.
    pub preset_path: String,
    pub preset_count: u32,
    pub adapter: String,
    /// How frames reach the screen: "iosurface" (no copies, macOS), "dxgi-shared" (no copies,
    /// Windows), "vulkan-shared" (no copies, Linux), "surface" (no copies, Android: presented
    /// into the texture's window), "readback", or empty before the render thread runs.
    pub transport: String,
    pub last_error: Option<String>,
}

/// The texture that shows the visualizer (macOS), sent once it started and whenever another one
/// replaces it. Show it, then confirm with `VisualizerTextureShownRequest`.
#[derive(Serialize, Deserialize, RustSignal)]
pub struct VisualizerTextureResponse {
    pub texture_id: i64,
}

/// The app shows the texture `texture_id`: the textures it replaced can go.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct VisualizerTextureShownRequest {
    pub texture_id: i64,
}

#[derive(Serialize, Deserialize, DartSignal)]
pub struct ListVisualizerPresetsRequest {}

#[derive(Serialize, Deserialize, DartSignal)]
pub struct ListVisualizerCreditsRequest {}

/// What the assets of one built-in scene owe their makers.
#[derive(Serialize, Deserialize, SignalPiece)]
pub struct VisualizerCredit {
    /// The scene's name.
    pub scene: String,
    /// The `source` of each asset of the scene that has one, as the scene writes it in English:
    /// the work, its authors, its licence and what was changed.
    pub assets: Vec<String>,
}

/// The credits of the built-in scenes that have any.
#[derive(Serialize, Deserialize, RustSignal)]
pub struct ListVisualizerCreditsResponse {
    pub credits: Vec<VisualizerCredit>,
}

#[derive(Serialize, Deserialize, SignalPiece)]
pub struct VisualizerPresetEntry {
    pub name: String,
    pub path: String,
    /// The folder the scene is in, relative to its scene folder; empty for a built-in scene.
    pub group: String,
    /// Whether the scene is compiled into the program (its `path` is `builtin:<name>`).
    pub builtin: bool,
}

/// Every scene, the built-in ones, the cover wall (path `app:cover-wall`, which the app draws and
/// the visualizer leaves blank) and those of the scene folders, by group and name.
#[derive(Serialize, Deserialize, RustSignal)]
pub struct ListVisualizerPresetsResponse {
    pub presets: Vec<VisualizerPresetEntry>,
}

/// Whether the visualizer copies what it sees in each frame for the in-app lab
/// ([`VisualizerLabResponse`]). Off by default, and the lab sends it on when its panel opens and
/// off when it closes: the app pays nothing for it otherwise. Turning it off drops what the lab
/// had collected.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct SetVisualizerLabRequest {
    pub enabled: bool,
}

/// How often the camera director cuts.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualizerLabPace {
    Sections,
    Calm,
    Normal,
    Busy,
}

/// The fallback level the music is at: A has a chart and a followed beat, B a chart alone, C
/// neither.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualizerLabLevel {
    A,
    B,
    C,
}

/// The accents of a cut-in.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualizerLabCutIn {
    Streaks,
    Halftone,
    Echo,
}

/// How a scene switch or a dressed camera cut looks.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualizerLabTransition {
    Cut,
    Fade,
    Dissolve,
    /// The band with the cover, in this style.
    CutIn {
        style: VisualizerLabCutIn,
    },
    Flash,
    Glitch,
    Mosaic,
}

/// Lens effects a lab turns on at every moment of the music, as if the scene listed them.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisualizerLabLens {
    pub flare: bool,
    pub leak: bool,
    pub glitch: bool,
    pub mosaic: bool,
}

/// Something the lab tells the visualizer to do now. Where a command carries an option, setting
/// it replaces what the visualizer chooses by itself, and none hands the choice back.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq, Eq)]
pub enum VisualizerLabCommand {
    /// The camera director cuts now, as if a cut were due.
    Cut,
    /// The chance in percent (0 to 100) that every moment of the music gets an effect, in place
    /// of the user's setting; none for the setting again.
    EffectChance { percent: Option<u8> },
    /// Cut to the shot at `index` (from 0) and hold it, so that the director cuts no more, until
    /// none.
    Shot { index: Option<u32> },
    /// Change to the look at `index` (from 0) now and hold it, so that the runtime changes the
    /// look no more, until none.
    Look { index: Option<u32> },
    /// Switch to the next scene now, through this transition.
    NextScene { transition: VisualizerLabTransition },
    /// Step the pace of the scene on screen: its own, then sections, calm, normal and busy, then
    /// its own again.
    CyclePace,
    /// The camera director cuts now, dressed with this transition.
    CutDressed { transition: VisualizerLabTransition },
    /// Start a lens flare now.
    Flare,
    /// Start a light leak now.
    Leak,
    /// Play a glitch over the picture now.
    LensGlitch,
    /// Play a mosaic over the picture now.
    LensMosaic,
    /// Every scene switch takes this transition, whatever it asks for; none leaves it be.
    SwitchWith {
        transition: Option<VisualizerLabTransition>,
    },
    /// Every camera cut of the director is dressed with this transition; none leaves it be.
    CutsWith {
        transition: Option<VisualizerLabTransition>,
    },
    /// Every section start that lifts the energy gets a burst of cuts (`Some(true)`), or none
    /// does (`Some(false)`); none leaves it to the visualizer.
    BurstsWith { bursts: Option<bool> },
    /// These lens effects start at every moment, as if every scene listed them, until set again
    /// with none on.
    LensWith { lens: VisualizerLabLens },
    /// The scene on screen uses this pace in place of its own; none for its own again.
    PaceWith { pace: Option<VisualizerLabPace> },
    /// Hold the music at this fallback level, which can only take it down from what the song
    /// has; none leaves it be.
    ForceLevel { level: Option<VisualizerLabLevel> },
    /// Start a burst of cuts now: the director cuts on each of the next 4 beats.
    Burst,
}

/// Tell the visualizer to do something now, for the lab. The command is dropped (and logged) when
/// the visualizer is not running.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct VisualizerLabCommandRequest {
    pub command: VisualizerLabCommand,
}

/// What the visualizer decided or was told to do, at the song time it happened (seconds).
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub enum VisualizerLabEventKind {
    /// The camera cut to a shot.
    Cut {
        shot: String,
        /// "wide", "medium", "close", "detail" or "top".
        size: String,
        /// 0 for a normal cut, else the position in a burst.
        fill: u32,
        /// The cut lands on a strong transition.
        strong: bool,
        /// The view jumps, rather than glides.
        hard: bool,
        /// The transition that dresses the cut, by name.
        dress: Option<String>,
        /// The scene's first shot, which it begins with.
        first: bool,
    },
    /// The look changed to `look`.
    Look { look: String },
    /// A moment of the music played an effect. `strength` is "medium" or "strong"; `effect` is
    /// what it did by name, none when the scene lists nothing that suits it.
    Moment {
        strength: String,
        effect: Option<String>,
    },
    /// A lab's command, as the visualizer applied it, in words.
    Lab { command: String },
}

#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerLabEvent {
    /// Song seconds.
    pub time: f64,
    pub kind: VisualizerLabEventKind,
}

/// One of the scene's shots.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerLabShot {
    /// The shot's name, empty if it has none.
    pub name: String,
    /// "wide", "medium", "close", "detail" or "top".
    pub size: String,
    /// Its place in the burst (1 to 4) for a burst shot; 0 for an ordinary one.
    pub burst: u32,
    /// Whether the song still owes the shot a cut.
    pub owed: bool,
}

/// The camera director of the scene on screen.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerLabDirector {
    /// The shot on, as a place in `shots`, and its name.
    pub shot_index: u32,
    pub shot_name: String,
    /// "wide", "medium", "close", "detail" or "top".
    pub size: String,
    /// How lively the section is: "quiet", "normal" or "high".
    pub tier: String,
    /// How often the director cuts, as "sections", "calm", "normal" or "busy": the pace in
    /// effect, and the scene's own.
    pub pace: String,
    pub own_pace: String,
    /// Seconds since the shot began, and its planned length (-1 when unknown).
    pub age: f32,
    pub length: f32,
    /// Whether the shot is held.
    pub held: bool,
    /// The scene's shots in order, which the numbers of `Shot` count.
    pub shots: Vec<VisualizerLabShot>,
}

/// The looks of the scene on screen.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerLabLooks {
    /// The scene's looks in order, which the numbers of `Look` count; a look without a name is
    /// empty.
    pub names: Vec<String>,
    /// The look on, and the one before it (places in `names`).
    pub on: u32,
    pub prev: u32,
    /// How far the fade from the one before is, 0 to 1.
    pub mix: f32,
    /// Whether the look is held.
    pub held: bool,
}

/// What one music signal said in the last frame.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerLabSignal {
    pub name: String,
    /// The value the bindings read; for an event, its strength.
    pub value: f32,
    /// Whether the signal counts events.
    pub event: bool,
    /// Whether the signal means something at the music's level.
    pub meaningful: bool,
}

/// An input of the scene on screen and the value it has now.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerLabInput {
    pub id: String,
    /// The label for people, else the id.
    pub label: String,
    /// What its bindings made of it in the last frame; its resting value when nothing drives it.
    pub value: f32,
}

/// The transitions and lens effects the scene on screen declares.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerLabEffects {
    /// What may dress a camera cut, by name.
    pub cut: Vec<String>,
    /// The lens effects, by name.
    pub lens: Vec<String>,
}

/// What a lab has set, as the visualizer last saw it; none or false everywhere is the
/// visualizer's own choices.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerLabOverrides {
    /// The chance in percent every moment gets an effect.
    pub chance: Option<u8>,
    /// Whether every lifting section start gets a burst (`Some(true)`) or none does
    /// (`Some(false)`).
    pub bursts: Option<bool>,
    pub level: Option<VisualizerLabLevel>,
    /// The pace in place of the scene's own, when a lab set one and the scene has a director.
    pub pace: Option<String>,
    /// What every scene switch takes, and what dresses every camera cut.
    pub switches: Option<VisualizerLabTransition>,
    pub cuts: Option<VisualizerLabTransition>,
    /// The lens effects on at every moment.
    pub lens: VisualizerLabLens,
}

/// One section of the song's chart.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerLabSection {
    /// Seconds.
    pub start: f64,
    pub end: f64,
    pub label: String,
    pub energy: f32,
}

/// What the visualizer saw in its last frame, for the live readout of the lab.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerLabNow {
    /// The fallback level the music is at: "A", "B" or "C".
    pub level: String,
    /// None when the scene declares no shots.
    pub director: Option<VisualizerLabDirector>,
    /// None when the scene declares no looks.
    pub looks: Option<VisualizerLabLooks>,
    pub signals: Vec<VisualizerLabSignal>,
    pub inputs: Vec<VisualizerLabInput>,
    pub effects: VisualizerLabEffects,
    pub overrides: VisualizerLabOverrides,
    /// The sections of the song's chart, once it is known.
    pub sections: Vec<VisualizerLabSection>,
}

/// Asked about every 100 ms while the lab's panel is open.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct VisualizerLabRequest {}

/// `inspecting` says whether the lab is on (see `SetVisualizerLabRequest`); `now` is none until a
/// frame was drawn. Ask for the plan again when `plan_version` is not the one last seen.
#[derive(Serialize, Deserialize, RustSignal)]
pub struct VisualizerLabResponse {
    pub inspecting: bool,
    /// Song seconds.
    pub song_time: f64,
    pub plan_version: u64,
    /// What happened since the last response, oldest first.
    pub events: Vec<VisualizerLabEvent>,
    pub now: Option<VisualizerLabNow>,
}

/// Asked for when the `plan_version` of the lab's response changes.
#[derive(Serialize, Deserialize, DartSignal)]
pub struct VisualizerPlanRequest {}

/// One section of the planned song.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerPlanSection {
    /// Seconds.
    pub start: f64,
    pub end: f64,
    pub label: String,
    pub energy: f32,
    /// How lively the director takes it to be: "quiet", "normal" or "high".
    pub tier: String,
}

/// A planned cut of the camera.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerPlanCut {
    /// Song seconds.
    pub time: f64,
    pub shot: String,
    /// "wide", "medium", "close", "detail" or "top".
    pub size: String,
    /// 0 for a normal cut, else the position in a burst (1 to 4).
    pub fill: u32,
    pub strong: bool,
    /// The view jumps, rather than glides.
    pub hard: bool,
    /// The transition that dresses it, by name.
    pub dress: Option<String>,
    /// The scene's first shot, which it begins with.
    pub first: bool,
}

/// A planned change of look; the first one is the look the scene opens in.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerPlanLook {
    /// Song seconds.
    pub time: f64,
    pub look: String,
}

/// A planned moment of the music that plays an effect.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerPlanMoment {
    /// Song seconds.
    pub time: f64,
    /// "medium" or "strong".
    pub strength: String,
    /// What it plays, by name; none if the scene lists nothing that suits the moment.
    pub effect: Option<String>,
}

/// What the scene on screen does in the song on screen, played through ahead of time.
#[derive(Serialize, Deserialize, SignalPiece, Clone, Debug, PartialEq)]
pub struct VisualizerPlan {
    /// The song's file, and the name of the scene the plan is of.
    pub path: String,
    pub scene: String,
    /// Seconds.
    pub length: f64,
    pub sections: Vec<VisualizerPlanSection>,
    pub cuts: Vec<VisualizerPlanCut>,
    pub looks: Vec<VisualizerPlanLook>,
    pub moments: Vec<VisualizerPlanMoment>,
}

/// The plan in `version`, none while the lab is off, the song has no chart or the plan is still
/// being made.
#[derive(Serialize, Deserialize, RustSignal)]
pub struct VisualizerPlanResponse {
    pub version: u64,
    pub plan: Option<VisualizerPlan>,
}
