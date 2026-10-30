import 'dart:async';

import 'package:flutter/foundation.dart';

import '../bindings/bindings.dart';
import '../constants/configurations.dart';
import '../constants/settings_manager.dart';
import '../utils/file_storage/mac_secure_manager.dart';
import '../utils/rune_log.dart';
import '../utils/visualizer_preset_dirs.dart';
import '../utils/visualizer_supported.dart';

/// The cover art wall as a scene. Rust lists it with the others and draws
/// nothing while it is the scene on screen; the app shows the wall instead.
const kCoverWallScenePath = 'app:cover-wall';

/// A scene of the visualizer, with whether it may be picked.
@immutable
class VisualizerScene {
  const VisualizerScene({
    required this.path,
    required this.name,
    required this.group,
    required this.builtin,
    required this.enabled,
  });

  final String path;
  final String name;
  final String group;
  final bool builtin;
  final bool enabled;

  bool get isCoverWall => path == kCoverWallScenePath;

  VisualizerScene copyWith({bool? enabled}) => VisualizerScene(
    path: path,
    name: name,
    group: group,
    builtin: builtin,
    enabled: enabled ?? this.enabled,
  );
}

/// The scenes of the visualizer in the order the user arranged them, which of
/// them are checked, and whether they play in that order or shuffled. The
/// scenes come from Rust; the rest is saved in the settings.
class VisualizerScenesProvider extends ChangeNotifier {
  List<VisualizerScene> _scenes = const [];
  bool _shuffle = false;
  bool _loaded = false;
  bool _sceneShowing = false;
  Future<void>? _refreshing;
  bool _refreshAgain = false;

  VisualizerScenesProvider() {
    if (isVisualizerSupported) _load();
  }

  List<VisualizerScene> get scenes => _scenes;

  /// Whether each song gets a random scene instead of the next one in the
  /// order.
  bool get shuffle => _shuffle;

  /// Whether the cover wall page runs the visualizer: some checked scene is
  /// not the cover wall. With only the cover wall checked the page shows the
  /// plain wall and the visualizer never starts. False until the scenes are
  /// loaded.
  bool get showVisualizer =>
      isVisualizerSupported &&
      _loaded &&
      _scenes.any((scene) => scene.enabled && !scene.isCoverWall);

  /// Whether a scene other than the cover wall is on screen. Set by the
  /// visualizer surface; scenes draw on black.
  bool get sceneShowing => _sceneShowing;
  set sceneShowing(bool value) {
    if (value == _sceneShowing) return;
    _sceneShowing = value;
    notifyListeners();
  }

  Future<void> _load() async {
    try {
      // Bookmarks are resolved when the manager is created; wait for them
      // before Rust reads the saved folders.
      if (MacSecureManager.isApplePlatform()) {
        await MacSecureManager().completed;
      }
      final dirs = await loadVisualizerPresetDirs();
      if (dirs.isNotEmpty) {
        SetVisualizerPresetDirsRequest(paths: dirs).sendSignalToRust();
      }
      // Rust may not answer yet while the app starts; until it does, the
      // cover wall page shows the plain wall.
      for (var attempt = 0; attempt < 5 && !_loaded; attempt++) {
        await refresh();
      }
    } catch (e) {
      error$('Failed to load the visualizer scenes: $e');
    }
  }

  /// Asks Rust for the scenes and reconciles them with the saved order and
  /// unchecked scenes. Scenes seen for the first time (all of them on the
  /// first run, then new built-in scenes and those of a new folder) are
  /// checked, at the end of the order; on the first run the cover wall comes
  /// first. Calls that overlap share one run after the current one.
  Future<void> refresh() {
    final running = _refreshing;
    if (running != null) {
      _refreshAgain = true;
      return running;
    }
    return _refreshing = _runRefresh();
  }

  Future<void> _runRefresh() async {
    try {
      while (true) {
        await _refreshOnce();
        if (!_refreshAgain) break;
        _refreshAgain = false;
      }
    } finally {
      _refreshing = null;
    }
  }

  Future<void> _refreshOnce() async {
    final response = ListVisualizerPresetsResponse.rustSignalStream.first
        .timeout(const Duration(seconds: 2));
    ListVisualizerPresetsRequest().sendSignalToRust();
    final List<VisualizerPresetEntry> listed;
    try {
      listed = (await response).message.presets;
    } on TimeoutException {
      return;
    }

    _shuffle =
        await $settingsManager.getValue<bool>(kVisualizerShuffleScenesKey) ??
        false;
    final storedOrder = await _loadPaths(kVisualizerScenesKey);
    final storedDisabled = await _loadPaths(kVisualizerDisabledScenesKey);
    final byPath = {for (final entry in listed) entry.path: entry};

    final order = <String>[];
    if (storedOrder == null && byPath.containsKey(kCoverWallScenePath)) {
      order.add(kCoverWallScenePath);
    }
    order.addAll((storedOrder ?? const []).where(byPath.containsKey));
    for (final entry in listed) {
      if (!order.contains(entry.path)) order.add(entry.path);
    }
    final disabled = (storedDisabled ?? const []).toSet();

    _scenes = [
      for (final path in order)
        VisualizerScene(
          path: path,
          name: byPath[path]!.name,
          group: byPath[path]!.group,
          builtin: byPath[path]!.builtin,
          enabled: !disabled.contains(path),
        ),
    ];
    _loaded = true;
    await _save();
    notifyListeners();
    _sendChoice();
  }

  Future<List<String>?> _loadPaths(String key) async {
    final saved = await $settingsManager.getValue<List>(key);
    if (saved == null) return null;
    return [
      for (final path in saved)
        if (path is String) path,
    ];
  }

  Future<void> _save() async {
    await $settingsManager.setValue(
      kVisualizerScenesKey,
      _scenes.map((scene) => scene.path).toList(),
    );
    await $settingsManager.setValue(
      kVisualizerDisabledScenesKey,
      _disabledPaths(),
    );
  }

  List<String> _disabledPaths() => [
    for (final scene in _scenes)
      if (!scene.enabled) scene.path,
  ];

  void _sendChoice() {
    SetVisualizerScenesRequest(
      order: _scenes.map((scene) => scene.path).toList(),
      disabled: _disabledPaths(),
      shuffle: _shuffle,
    ).sendSignalToRust();
  }

  void setShuffle(bool shuffle) {
    if (shuffle == _shuffle) return;
    _shuffle = shuffle;
    $settingsManager.setValue(kVisualizerShuffleScenesKey, shuffle);
    notifyListeners();
    _sendChoice();
  }

  /// Moves a scene, with the semantics of `ReorderableColumn.onReorder`.
  void reorder(int oldIndex, int newIndex) {
    if (oldIndex < 0 ||
        oldIndex >= _scenes.length ||
        newIndex < 0 ||
        newIndex >= _scenes.length) {
      return;
    }
    final scenes = [..._scenes];
    scenes.insert(newIndex, scenes.removeAt(oldIndex));
    _scenes = scenes;
    _save();
    notifyListeners();
    _sendChoice();
  }

  /// Checks or unchecks a scene. The last checked scene cannot be unchecked.
  void setEnabled(String path, bool enabled) {
    final index = _scenes.indexWhere((scene) => scene.path == path);
    if (index < 0 || _scenes[index].enabled == enabled) return;
    if (!enabled && _scenes.where((scene) => scene.enabled).length <= 1) {
      return;
    }
    final scenes = [..._scenes];
    scenes[index] = scenes[index].copyWith(enabled: enabled);
    _scenes = scenes;
    _save();
    notifyListeners();
    _sendChoice();
  }
}
