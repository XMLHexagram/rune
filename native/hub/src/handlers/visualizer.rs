use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use ::database::connection::{MainDbConnection, RecommendationDbConnection};
use ::fsio::FsIo;
use ::playback::player::Playable;
use ::visualizer::{
    CutInStyle, Inspect, LabCommand, LabEvent, LabEventKind, LabLens, LabPace, LabTransition,
    Level, Location, PcmInput, Transport,
};

use crate::{
    Session, Signal, flutter_bridge,
    messages::*,
    utils::{GlobalParams, ParamsExtractor},
    visualizer::{VISUALIZER, provide_covers},
};

/// Errors of the visualizer controls go to the status (`last_error`) and the Log page, under
/// `operation`, instead of crashing the request loop, since a missing scene or GPU is an ordinary
/// condition.
fn report(operation: &'static str, result: Result<()>) -> Result<Option<()>> {
    if let Err(e) = result {
        VISUALIZER.report_error(operation, format!("{e:#}"));
    }
    Ok(Some(()))
}

/// Held through a start, a stop and a resize, which are handled concurrently: a surface that
/// replaces another sends a stop and then a start, and the start must wait for the stop to give
/// the audio back; a resize that lands while a start prepares its window must wait for the start,
/// or it finds nothing to resize and is lost.
static STARTING_OR_STOPPING: Mutex<()> = Mutex::const_new(());

impl ParamsExtractor for StartVisualizerRequest {
    type Params = (
        Arc<Mutex<dyn Playable>>,
        Arc<FsIo>,
        Arc<MainDbConnection>,
        Arc<RecommendationDbConnection>,
    );

    fn extract_params(&self, all_params: &GlobalParams) -> Self::Params {
        (
            Arc::clone(&all_params.player),
            Arc::clone(&all_params.fsio),
            Arc::clone(&all_params.main_db),
            Arc::clone(&all_params.recommend_db),
        )
    }
}

impl Signal for StartVisualizerRequest {
    type Params = (
        Arc<Mutex<dyn Playable>>,
        Arc<FsIo>,
        Arc<MainDbConnection>,
        Arc<RecommendationDbConnection>,
    );
    type Response = ();

    async fn handle(
        &self,
        (player, fsio, main_db, recommend_db): Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let _turn = STARTING_OR_STOPPING.lock().await;
        crate::visualizer_log::write_to(Arc::clone(&main_db));
        provide_covers(fsio, main_db, recommend_db);
        let tap = player.lock().await.pcm_tap();
        // A restart: take the audio back from the running visualizer first.
        if let Some(input) = VISUALIZER.stop() {
            tap.restore_consumer(input.consumer);
        }
        let Some(consumer) = tap.take_consumer() else {
            return report("start", Err(anyhow::anyhow!("The PCM tap is in use")));
        };
        tap.set_enabled(true);
        let input = PcmInput {
            consumer,
            channels: tap.channels(),
            sample_rate: tap.sample_rate(),
        };
        flutter_bridge::prepare_visualizer(dart_signal.width, dart_signal.height).await;
        let result = VISUALIZER.start(
            input,
            dart_signal.width,
            dart_signal.height,
            dart_signal.fps,
        );
        if result.is_err() {
            tap.set_enabled(false);
            return report("start", result);
        }
        flutter_bridge::visualizer_started().await;
        Ok(Some(()))
    }
}

impl ParamsExtractor for StopVisualizerRequest {
    type Params = (Arc<Mutex<dyn Playable>>,);

    fn extract_params(&self, all_params: &GlobalParams) -> Self::Params {
        (Arc::clone(&all_params.player),)
    }
}

impl Signal for StopVisualizerRequest {
    type Params = (Arc<Mutex<dyn Playable>>,);
    type Response = ();

    async fn handle(
        &self,
        (player,): Self::Params,
        _session: Option<Session>,
        _dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let _turn = STARTING_OR_STOPPING.lock().await;
        let tap = player.lock().await.pcm_tap();
        tap.set_enabled(false);
        if let Some(input) = flutter_bridge::stop_visualizer().await {
            tap.restore_consumer(input.consumer);
        }
        Ok(Some(()))
    }
}

impl ParamsExtractor for ResizeVisualizerRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for ResizeVisualizerRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let _turn = STARTING_OR_STOPPING.lock().await;
        report(
            "resize",
            flutter_bridge::resize_visualizer(dart_signal.width, dart_signal.height).await,
        )
    }
}

impl ParamsExtractor for SetVisualizerPresetDirsRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for SetVisualizerPresetDirsRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let dirs: Vec<PathBuf> = dart_signal.paths.iter().map(PathBuf::from).collect();
        report("scan_scenes", VISUALIZER.set_preset_dirs(&dirs).map(|_| ()))
    }
}

impl ParamsExtractor for SetVisualizerAccentRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for SetVisualizerAccentRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        VISUALIZER.set_accent(dart_signal.argb);
        Ok(Some(()))
    }
}

impl ParamsExtractor for SetVisualizerVisibleRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for SetVisualizerVisibleRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        VISUALIZER.set_visible(dart_signal.visible);
        Ok(Some(()))
    }
}

impl ParamsExtractor for LoadVisualizerPresetRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for LoadVisualizerPresetRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        report(
            "load_scene",
            VISUALIZER.load_preset(Location::parse(&dart_signal.path)),
        )
    }
}

impl ParamsExtractor for NextVisualizerPresetRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for NextVisualizerPresetRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        _dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        report("switch_scene", VISUALIZER.next_preset())
    }
}

impl ParamsExtractor for PreviousVisualizerPresetRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for PreviousVisualizerPresetRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        _dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        report("switch_scene", VISUALIZER.previous_preset())
    }
}

impl ParamsExtractor for RandomVisualizerPresetRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for RandomVisualizerPresetRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        _dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        report("switch_scene", VISUALIZER.random_preset())
    }
}

impl ParamsExtractor for VisualizerStatusRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for VisualizerStatusRequest {
    type Params = ();
    type Response = VisualizerStatusResponse;

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        _dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let status = VISUALIZER.status();
        Ok(Some(VisualizerStatusResponse {
            running: status.running,
            width: status.width,
            height: status.height,
            target_fps: status.target_fps,
            measured_fps: status.measured_fps,
            preset_name: status.preset_name,
            preset_path: status.preset_path,
            preset_count: u32::try_from(status.preset_count).unwrap_or(u32::MAX),
            adapter: status.adapter,
            transport: match status.transport {
                Some(Transport::IoSurface) => "iosurface",
                Some(Transport::Readback) => "readback",
                Some(Transport::DxgiShared) => "dxgi-shared",
                Some(Transport::VulkanShared) => "vulkan-shared",
                Some(Transport::Surface) => "surface",
                None => "",
            }
            .to_string(),
            last_error: status.last_error,
        }))
    }
}

impl ParamsExtractor for VisualizerTextureShownRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for VisualizerTextureShownRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        flutter_bridge::texture_shown(dart_signal.texture_id).await;
        Ok(Some(()))
    }
}

impl ParamsExtractor for ListVisualizerCreditsRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for ListVisualizerCreditsRequest {
    type Params = ();
    type Response = ListVisualizerCreditsResponse;

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        _dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let credits = ::visualizer::builtin_credits()
            .into_iter()
            .map(|credit| VisualizerCredit {
                scene: credit.scene,
                assets: credit.assets,
            })
            .collect();
        Ok(Some(ListVisualizerCreditsResponse { credits }))
    }
}

impl ParamsExtractor for SetVisualizerScenesRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for SetVisualizerScenesRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let parse = |paths: &[String]| paths.iter().map(|path| Location::parse(path)).collect();
        VISUALIZER.set_scene_choice(parse(&dart_signal.order), parse(&dart_signal.disabled));
        if let Some(last) = &dart_signal.last {
            VISUALIZER.resume_from(Location::parse(last));
        }
        Ok(Some(()))
    }
}

impl ParamsExtractor for ImportVisualizerSceneRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for ImportVisualizerSceneRequest {
    type Params = ();
    type Response = ImportVisualizerSceneResponse;

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let file = PathBuf::from(&dart_signal.file);
        let dir = PathBuf::from(&dart_signal.dir);
        let imported = {
            let (file, dir) = (file.clone(), dir.clone());
            // A panic in the task is an answer too: the request must not end in a crash response.
            tokio::task::spawn_blocking(move || ::visualizer::package::import(&file, &dir))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("The import stopped unexpectedly: {e}")))
        };
        let (path, error) = match imported {
            Ok(path) => {
                let imported = format!("Imported {} into {}", file.display(), path.display());
                log::info!("{imported}");
                crate::visualizer_log::diagnostic("import_scene", imported);
                rescan(&dir);
                (Some(path.to_string_lossy().into_owned()), None)
            }
            Err(e) => {
                log::error!("Failed to import the scene {}: {e:#}", file.display());
                crate::visualizer_log::error("import_scene", format!("{}: {e:#}", file.display()));
                (None, Some(format!("{e:#}")))
            }
        };
        Ok(Some(ImportVisualizerSceneResponse {
            file: dart_signal.file.clone(),
            path,
            error,
        }))
    }
}

impl ParamsExtractor for RemoveVisualizerSceneRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for RemoveVisualizerSceneRequest {
    type Params = ();
    type Response = RemoveVisualizerSceneResponse;

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let path = PathBuf::from(&dart_signal.path);
        let dir = PathBuf::from(&dart_signal.dir);
        let removed = {
            let (path, dir) = (path.clone(), dir.clone());
            // A panic in the task is an answer too: the request must not end in a crash response.
            tokio::task::spawn_blocking(move || ::visualizer::package::remove(&path, &dir))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("The removal stopped unexpectedly: {e}")))
        };
        let error = match removed {
            Ok(()) => {
                log::info!("Deleted the scene {}", path.display());
                rescan(&dir);
                None
            }
            Err(e) => {
                log::error!("Failed to delete the scene {}: {e:#}", path.display());
                crate::visualizer_log::error("remove_scene", format!("{}: {e:#}", path.display()));
                Some(format!("{e:#}"))
            }
        };
        Ok(Some(RemoveVisualizerSceneResponse {
            path: dart_signal.path.clone(),
            error,
        }))
    }
}

/// List the scenes of `dir` again after a scene came or went.
fn rescan(dir: &std::path::Path) {
    if let Err(e) = VISUALIZER.set_preset_dirs(&[dir.to_path_buf()]) {
        VISUALIZER.report_error("scan_scenes", format!("{e:#}"));
    }
}

impl ParamsExtractor for SetVisualizerLogRequest {
    type Params = (Arc<MainDbConnection>,);

    fn extract_params(&self, all_params: &GlobalParams) -> Self::Params {
        (Arc::clone(&all_params.main_db),)
    }
}

impl Signal for SetVisualizerLogRequest {
    type Params = (Arc<MainDbConnection>,);
    type Response = ();

    async fn handle(
        &self,
        (main_db,): Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        crate::visualizer_log::set_diagnostics(dart_signal.enabled);
        crate::visualizer_log::write_to(main_db);
        Ok(Some(()))
    }
}

/// What [`visualizer::probe_gpu`] found, once: the GPU does not change while the app runs.
static GPU: tokio::sync::OnceCell<std::result::Result<String, String>> =
    tokio::sync::OnceCell::const_new();

impl ParamsExtractor for VisualizerSupportRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for VisualizerSupportRequest {
    type Params = ();
    type Response = VisualizerSupportResponse;

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        _dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let gpu = GPU
            .get_or_init(|| async {
                let probed = tokio::task::spawn_blocking(::visualizer::probe_gpu).await;
                let gpu = match probed {
                    Ok(Ok(adapter)) => Ok(adapter),
                    Ok(Err(e)) => Err(format!("{e:#}")),
                    Err(e) => Err(format!("The GPU probe failed: {e}")),
                };
                match &gpu {
                    Ok(adapter) => {
                        log::info!("The visualizer can run on {adapter}");
                        crate::visualizer_log::diagnostic(
                            "probe_gpu",
                            format!("The visualizer can run on {adapter}"),
                        );
                    }
                    Err(problem) => {
                        log::error!("The visualizer cannot run: {problem}");
                        crate::visualizer_log::error("probe_gpu", problem.clone());
                    }
                }
                gpu
            })
            .await;
        Ok(Some(match gpu {
            Ok(adapter) => VisualizerSupportResponse {
                supported: true,
                adapter: adapter.clone(),
                problem: None,
            },
            Err(problem) => VisualizerSupportResponse {
                supported: false,
                adapter: String::new(),
                problem: Some(problem.clone()),
            },
        }))
    }
}

impl ParamsExtractor for ListVisualizerPresetsRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for ListVisualizerPresetsRequest {
    type Params = ();
    type Response = ListVisualizerPresetsResponse;

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        _dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let presets = VISUALIZER
            .list_presets()
            .into_iter()
            .map(|info| VisualizerPresetEntry {
                name: info.name,
                path: info.location.to_string(),
                group: info.group,
                builtin: matches!(info.location, Location::Builtin(_)),
            })
            .collect();
        Ok(Some(ListVisualizerPresetsResponse { presets }))
    }
}

impl ParamsExtractor for SetVisualizerLabRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for SetVisualizerLabRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        VISUALIZER.set_inspecting(dart_signal.enabled);
        Ok(Some(()))
    }
}

impl From<VisualizerLabPace> for LabPace {
    fn from(pace: VisualizerLabPace) -> Self {
        match pace {
            VisualizerLabPace::Sections => Self::Sections,
            VisualizerLabPace::Calm => Self::Calm,
            VisualizerLabPace::Normal => Self::Normal,
            VisualizerLabPace::Busy => Self::Busy,
        }
    }
}

impl From<VisualizerLabLevel> for Level {
    fn from(level: VisualizerLabLevel) -> Self {
        match level {
            VisualizerLabLevel::A => Self::A,
            VisualizerLabLevel::B => Self::B,
            VisualizerLabLevel::C => Self::C,
        }
    }
}

impl From<Level> for VisualizerLabLevel {
    fn from(level: Level) -> Self {
        match level {
            Level::A => Self::A,
            Level::B => Self::B,
            Level::C => Self::C,
        }
    }
}

impl From<VisualizerLabCutIn> for CutInStyle {
    fn from(style: VisualizerLabCutIn) -> Self {
        match style {
            VisualizerLabCutIn::Streaks => Self::Streaks,
            VisualizerLabCutIn::Halftone => Self::Halftone,
            VisualizerLabCutIn::Echo => Self::Echo,
        }
    }
}

impl From<CutInStyle> for VisualizerLabCutIn {
    fn from(style: CutInStyle) -> Self {
        match style {
            CutInStyle::Streaks => Self::Streaks,
            CutInStyle::Halftone => Self::Halftone,
            CutInStyle::Echo => Self::Echo,
        }
    }
}

impl From<VisualizerLabTransition> for LabTransition {
    fn from(transition: VisualizerLabTransition) -> Self {
        match transition {
            VisualizerLabTransition::Cut => Self::Cut,
            VisualizerLabTransition::Fade => Self::Fade,
            VisualizerLabTransition::Dissolve => Self::Dissolve,
            VisualizerLabTransition::CutIn { style } => Self::CutIn(style.into()),
            VisualizerLabTransition::Flash => Self::Flash,
            VisualizerLabTransition::Glitch => Self::Glitch,
            VisualizerLabTransition::Mosaic => Self::Mosaic,
        }
    }
}

impl From<LabTransition> for VisualizerLabTransition {
    fn from(transition: LabTransition) -> Self {
        match transition {
            LabTransition::Cut => Self::Cut,
            LabTransition::Fade => Self::Fade,
            LabTransition::Dissolve => Self::Dissolve,
            LabTransition::CutIn(style) => Self::CutIn {
                style: style.into(),
            },
            LabTransition::Flash => Self::Flash,
            LabTransition::Glitch => Self::Glitch,
            LabTransition::Mosaic => Self::Mosaic,
        }
    }
}

impl From<VisualizerLabLens> for LabLens {
    fn from(lens: VisualizerLabLens) -> Self {
        Self {
            flare: lens.flare,
            leak: lens.leak,
            glitch: lens.glitch,
            mosaic: lens.mosaic,
        }
    }
}

impl From<LabLens> for VisualizerLabLens {
    fn from(lens: LabLens) -> Self {
        Self {
            flare: lens.flare,
            leak: lens.leak,
            glitch: lens.glitch,
            mosaic: lens.mosaic,
        }
    }
}

impl From<VisualizerLabCommand> for LabCommand {
    fn from(command: VisualizerLabCommand) -> Self {
        match command {
            VisualizerLabCommand::Cut => Self::Cut,
            VisualizerLabCommand::EffectChance { percent } => Self::EffectChance(percent),
            VisualizerLabCommand::Shot { index } => Self::Shot(index),
            VisualizerLabCommand::Look { index } => Self::Look(index),
            VisualizerLabCommand::NextScene { transition } => Self::NextScene(transition.into()),
            VisualizerLabCommand::CyclePace => Self::CyclePace,
            VisualizerLabCommand::CutDressed { transition } => Self::CutDressed(transition.into()),
            VisualizerLabCommand::Flare => Self::Flare,
            VisualizerLabCommand::Leak => Self::Leak,
            VisualizerLabCommand::LensGlitch => Self::LensGlitch,
            VisualizerLabCommand::LensMosaic => Self::LensMosaic,
            VisualizerLabCommand::SwitchWith { transition } => {
                Self::SwitchWith(transition.map(Into::into))
            }
            VisualizerLabCommand::CutsWith { transition } => {
                Self::CutsWith(transition.map(Into::into))
            }
            VisualizerLabCommand::BurstsWith { bursts } => Self::BurstsWith(bursts),
            VisualizerLabCommand::LensWith { lens } => Self::LensWith(lens.into()),
            VisualizerLabCommand::PaceWith { pace } => Self::PaceWith(pace.map(Into::into)),
            VisualizerLabCommand::ForceLevel { level } => Self::ForceLevel(level.map(Into::into)),
            VisualizerLabCommand::Burst => Self::Burst,
        }
    }
}

impl ParamsExtractor for VisualizerLabCommandRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for VisualizerLabCommandRequest {
    type Params = ();
    type Response = ();

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        // A visualizer that is not running has nothing to tell.
        if let Err(e) = VISUALIZER.lab(dart_signal.command.clone().into()) {
            log::warn!("The visualizer lab command was dropped: {e:#}");
        }
        Ok(Some(()))
    }
}

/// The lab's events as messages; an event of a kind this build does not know is left out.
fn lab_event(event: LabEvent) -> Option<VisualizerLabEvent> {
    let kind = match event.kind {
        LabEventKind::Cut {
            shot,
            size,
            fill,
            strong,
            hard,
            dress,
            first,
        } => VisualizerLabEventKind::Cut {
            shot,
            size,
            fill,
            strong,
            hard,
            dress,
            first,
        },
        LabEventKind::Look { look } => VisualizerLabEventKind::Look { look },
        LabEventKind::Moment { strength, effect } => {
            VisualizerLabEventKind::Moment { strength, effect }
        }
        LabEventKind::Lab { command } => VisualizerLabEventKind::Lab { command },
        _ => return None,
    };
    Some(VisualizerLabEvent {
        time: event.time,
        kind,
    })
}

/// The live readout of a frame the visualizer saw.
fn lab_now(inspect: Inspect) -> VisualizerLabNow {
    // `Inspect` does not carry the level: it is what the music's values say (see `Level::of`).
    let level = if !inspect.music.has_chart {
        Level::C
    } else if inspect.music.locked {
        Level::A
    } else {
        Level::B
    };
    let overrides = inspect.overrides;
    let pace = inspect
        .director
        .as_ref()
        .filter(|director| director.pace_overridden)
        .map(|director| director.pace.clone());
    let inputs = inspect
        .inputs
        .into_iter()
        .map(|input| {
            let value = inspect
                .bindings
                .iter()
                .rev()
                .find(|binding| binding.target == input.id)
                .map_or(input.rest, |binding| binding.output);
            VisualizerLabInput {
                id: input.id,
                label: input.label,
                value,
            }
        })
        .collect();
    VisualizerLabNow {
        level: level.name().to_string(),
        director: inspect.director.map(|director| VisualizerLabDirector {
            shot_index: director.shot_index,
            shot_name: director.shot_name,
            size: director.size,
            tier: director.tier,
            pace: director.pace,
            own_pace: director.own_pace,
            age: director.age,
            length: director.length,
            held: director.held,
            shots: director
                .shots
                .into_iter()
                .map(|shot| VisualizerLabShot {
                    name: shot.name,
                    size: shot.size,
                    burst: shot.burst,
                    owed: shot.owed,
                })
                .collect(),
        }),
        looks: inspect.looks.map(|looks| VisualizerLabLooks {
            names: looks.names,
            on: looks.on,
            prev: looks.prev,
            mix: looks.mix,
            held: looks.held,
        }),
        signals: inspect
            .signals
            .into_iter()
            .map(|signal| VisualizerLabSignal {
                name: signal.name,
                value: signal.value,
                event: signal.event,
                meaningful: signal.meaningful,
            })
            .collect(),
        inputs,
        effects: VisualizerLabEffects {
            cut: inspect.effects.cut,
            lens: inspect.effects.lens,
        },
        overrides: VisualizerLabOverrides {
            chance: inspect.chance,
            bursts: overrides.bursts,
            level: overrides.level.map(Into::into),
            pace,
            switches: overrides.switch.map(Into::into),
            cuts: overrides.cuts.map(Into::into),
            lens: overrides.lens.into(),
        },
        sections: inspect
            .sections
            .into_iter()
            .map(|section| VisualizerLabSection {
                start: section.start,
                end: section.end,
                label: section.label,
                energy: section.energy,
            })
            .collect(),
    }
}

impl ParamsExtractor for VisualizerLabRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for VisualizerLabRequest {
    type Params = ();
    type Response = VisualizerLabResponse;

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        _dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        let inspect = VISUALIZER.inspect();
        Ok(Some(VisualizerLabResponse {
            inspecting: VISUALIZER.is_inspecting(),
            song_time: inspect
                .as_ref()
                .map_or(0.0, |inspect| f64::from(inspect.music.time)),
            plan_version: VISUALIZER.plan_version(),
            events: VISUALIZER
                .drain_lab_events()
                .into_iter()
                .filter_map(lab_event)
                .collect(),
            now: inspect.map(lab_now),
        }))
    }
}

impl ParamsExtractor for VisualizerPlanRequest {
    type Params = ();

    fn extract_params(&self, _all_params: &GlobalParams) -> Self::Params {}
}

impl Signal for VisualizerPlanRequest {
    type Params = ();
    type Response = VisualizerPlanResponse;

    async fn handle(
        &self,
        _: Self::Params,
        _session: Option<Session>,
        _dart_signal: &Self,
    ) -> Result<Option<Self::Response>> {
        // The version first: a plan made in between is then fetched again for its own version.
        let version = VISUALIZER.plan_version();
        let plan = VISUALIZER.plan().map(|plan| VisualizerPlan {
            path: plan.path.clone(),
            scene: plan.scene.clone(),
            length: plan.length,
            sections: plan
                .sections
                .iter()
                .map(|section| VisualizerPlanSection {
                    start: section.start,
                    end: section.end,
                    label: section.label.clone(),
                    energy: section.energy,
                    tier: section.tier.clone(),
                })
                .collect(),
            cuts: plan
                .cuts
                .iter()
                .map(|cut| VisualizerPlanCut {
                    time: cut.time,
                    shot: cut.shot.clone(),
                    size: cut.size.clone(),
                    fill: cut.fill,
                    strong: cut.strong,
                    hard: cut.hard,
                    dress: cut.dress.clone(),
                    first: cut.first,
                })
                .collect(),
            looks: plan
                .looks
                .iter()
                .map(|look| VisualizerPlanLook {
                    time: look.time,
                    look: look.look.clone(),
                })
                .collect(),
            moments: plan
                .moments
                .iter()
                .map(|moment| VisualizerPlanMoment {
                    time: moment.time,
                    strength: moment.strength.clone(),
                    effect: moment.effect.clone(),
                })
                .collect(),
        });
        Ok(Some(VisualizerPlanResponse { version, plan }))
    }
}
