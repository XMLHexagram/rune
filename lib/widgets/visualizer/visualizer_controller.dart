import 'package:flutter/foundation.dart';

import '../../bindings/bindings.dart';

/// What the corner of the visualizer shows: the name of a scene that just
/// started, or a problem.
@immutable
class VisualizerLabel {
  const VisualizerLabel(this.text, {this.isError = false});

  final String text;
  final bool isError;
}

/// The commands and the state of a [VisualizerSurface] for the widgets around
/// it, such as a context menu and keyboard shortcuts. The commands do nothing
/// while no surface is attached.
class VisualizerController {
  bool _attached = false;
  bool _locked = false;
  bool _menuOpen = false;
  bool _autoSwitchPaused = false;

  VisualizerStatusResponse? status;
  // Why the texture could not be set up.
  String? startError;
  bool hasPresets = false;

  /// Set by the surface whenever a scene from a folder starts or a problem
  /// comes up; null while the built-in scene shows.
  final ValueNotifier<VisualizerLabel?> label = ValueNotifier(null);

  /// The path of the scene on screen, from the status; null before the
  /// visualizer runs.
  final ValueNotifier<String?> scenePath = ValueNotifier(null);

  /// The user stopped the automatic scene switching.
  bool get locked => _locked;

  /// A new surface starts with a new render thread, so the old state is gone;
  /// only the lock carries over ([locked], as the user left it).
  void attach({bool locked = false}) {
    _attached = true;
    _locked = locked;
    _menuOpen = false;
    _autoSwitchPaused = false;
    status = null;
    startError = null;
    hasPresets = false;
    label.value = null;
    scenePath.value = null;
  }

  void detach() => _attached = false;

  void next() {
    if (!_attached) return;
    NextVisualizerPresetRequest().sendSignalToRust();
  }

  void previous() {
    if (!_attached) return;
    PreviousVisualizerPresetRequest().sendSignalToRust();
  }

  void random() {
    if (!_attached) return;
    RandomVisualizerPresetRequest().sendSignalToRust();
  }

  void loadPreset(String path) {
    if (!_attached) return;
    LoadVisualizerPresetRequest(path: path).sendSignalToRust();
  }

  void toggleLock() {
    _locked = !_locked;
    _syncAutoSwitchPaused();
  }

  void setMenuOpen(bool open) {
    _menuOpen = open;
    _syncAutoSwitchPaused();
  }

  /// Tells a render thread that just started whether the scene is locked: it
  /// starts unlocked.
  void sendLock() => _syncAutoSwitchPaused(force: true);

  // Rust does not switch scenes by itself while the scene is locked or the
  // menu is open, and takes the song that plays afterwards as the scene's own.
  void _syncAutoSwitchPaused({bool force = false}) {
    if (!_attached) return;
    final paused = _locked || _menuOpen;
    if (paused == _autoSwitchPaused && !force) return;
    _autoSwitchPaused = paused;
    SetVisualizerAutoSwitchPausedRequest(paused: paused).sendSignalToRust();
  }
}
