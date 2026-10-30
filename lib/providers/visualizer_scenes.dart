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

/// The scenes of the visualizer in the order the user arranged them, and
/// which of them are checked. The scenes come from Rust; the order and the
/// unchecked ones are saved in the settings.
class VisualizerScenesProvider extends ChangeNotifier {
  List<VisualizerScene> _scenes = const [];
  bool _loaded = false;
  bool _sceneShowing = false;
  Future<void>? _refreshing;
  bool _refreshAgain = false;
  bool _enableNewAgain = false;

  VisualizerScenesProvider() {
    if (isVisualizerSupported) _load();
  }

  List<VisualizerScene> get scenes => _scenes;

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
  /// unchecked scenes. New scenes are checked if [enableNew] is set or if the
  /// user already uses scenes other than the cover wall. Calls that overlap
  /// share one run after the current one.
  Future<void> refresh({bool enableNew = false}) {
    final running = _refreshing;
    if (running != null) {
      _refreshAgain = true;
      _enableNewAgain = _enableNewAgain || enableNew;
      return running;
    }
    return _refreshing = _runRefresh(enableNew);
  }

  Future<void> _runRefresh(bool enableNew) async {
    try {
      while (true) {
        await _refreshOnce(enableNew);
        if (!_refreshAgain) break;
        _refreshAgain = false;
        enableNew = _enableNewAgain;
        _enableNewAgain = false;
      }
    } finally {
      _refreshing = null;
    }
  }

  Future<void> _refreshOnce(bool enableNew) async {
    final response = ListVisualizerPresetsResponse.rustSignalStream.first
        .timeout(const Duration(seconds: 2));
    ListVisualizerPresetsRequest().sendSignalToRust();
    final List<VisualizerPresetEntry> listed;
    try {
      listed = (await response).message.presets;
    } on TimeoutException {
      return;
    }

    final storedOrder = await _loadPaths(kVisualizerScenesKey);
    final storedDisabled = await _loadPaths(kVisualizerDisabledScenesKey);
    final byPath = {for (final entry in listed) entry.path: entry};

    final order = <String>[];
    final disabled = <String>{};
    if (storedOrder == null) {
      // First run: earlier versions chose between the wall and the visualizer.
      final old = await $settingsManager.getValue<String>(kCoverWallDisplayKey);
      final all = old == 'visualizer';
      if (byPath.containsKey(kCoverWallScenePath)) {
        order.add(kCoverWallScenePath);
      }
      order.addAll(
        listed.map((e) => e.path).where((p) => p != kCoverWallScenePath),
      );
      if (!all) {
        disabled.addAll(order.where((p) => p != kCoverWallScenePath));
      }
      await $settingsManager.removeValue(kCoverWallDisplayKey);
    } else {
      final known = storedOrder.where(byPath.containsKey).toList();
      final knownDisabled = storedDisabled?.toSet() ?? <String>{};
      final usesScenes = known.any(
        (p) => p != kCoverWallScenePath && !knownDisabled.contains(p),
      );
      order.addAll(known);
      disabled.addAll(known.where(knownDisabled.contains));
      for (final entry in listed) {
        if (order.contains(entry.path)) continue;
        order.add(entry.path);
        if (!enableNew && !usesScenes) disabled.add(entry.path);
      }
    }

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
    _sendDisabled();
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

  void _sendDisabled() {
    SetVisualizerDisabledScenesRequest(
      paths: _disabledPaths(),
    ).sendSignalToRust();
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
    _sendDisabled();
  }
}
