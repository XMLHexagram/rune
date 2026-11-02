import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/scheduler.dart';

import '../bindings/bindings.dart';
import '../constants/configurations.dart';
import '../constants/settings_manager.dart';
import '../utils/visualizer_supported.dart';
import 'status.dart';

/// What a mark on the lab's timeline is. A jump is where the visualizer was put
/// at a song time without playing up to it (it opened, or the song was sought),
/// and went straight to the shot on.
enum LabMarkKind { cut, jump, look, moment, manual }

/// Something the visualizer did, or the user played by hand, at a song time.
@immutable
class LabMark {
  const LabMark({
    required this.time,
    required this.kind,
    this.size = '',
    this.fill = 0,
    this.strong = false,
    this.filled = false,
    this.label = '',
  });

  /// Song seconds.
  final double time;
  final LabMarkKind kind;

  /// "wide", "medium", "close", "detail" or "top", for a cut.
  final String size;

  /// 0 for a normal cut, else the position in a burst.
  final int fill;
  final bool strong;

  /// For a moment, whether it played an effect.
  final bool filled;
  final String label;
}

/// The lab of the visualizer: while it is on, the visualizer copies what it
/// decides and the lab asks for it by polling, to show the song's plan next to
/// what really happened. The setting is saved; the panel's state is not.
class VisualizerLabProvider extends ChangeNotifier {
  static const _openInterval = Duration(milliseconds: 100);
  static const _closedInterval = Duration(milliseconds: 500);
  // The lab asks the visualizer to collect again this often while it says it
  // does not (it restarts with the cover wall page).
  static const _reEnableEvery = Duration(seconds: 2);
  static const _maxMarks = 3000;
  // A song time this much behind the last one is a seek back.
  static const _backwardsJump = 0.3;

  bool _enabled = false;
  bool _panelOpen = false;
  VisualizerPlan? _plan;
  // The plan version the plan was fetched at; none asks again.
  int? _planVersion;
  VisualizerLabNow? _now;
  double? _songTime;
  final List<LabMark> _observed = [];
  final List<LabMark> _temporary = [];
  // The stretches of the song the visualizer was seen playing, as (from, to);
  // a jump starts another.
  final List<(double, double)> _watched = [];

  PlaybackStatusProvider? _status;
  String? _trackKey;
  Timer? _timer;
  bool _polling = false;
  bool _disposed = false;
  DateTime? _lastEnableSent;

  VisualizerLabProvider() {
    if (isVisualizerSupported) _load();
  }

  /// Whether the setting is on.
  bool get enabled => _enabled;

  /// Whether the panel on the cover wall page is showing.
  bool get panelOpen => _enabled && _panelOpen;

  VisualizerPlan? get plan => _plan;
  VisualizerLabNow? get now => _now;

  /// The song time of the last readout, in seconds.
  double? get songTime => _songTime;

  /// Marks from what the visualizer did, in time order.
  List<LabMark> get observed => List.unmodifiable(_observed);

  /// Marks of the effects the user played by hand.
  List<LabMark> get temporary => List.unmodifiable(_temporary);

  /// The stretches of the song the visualizer was seen playing, as (from, to)
  /// in song seconds, in the order they were played.
  List<(double, double)> get watched => List.unmodifiable(_watched);

  Future<void> _load() async {
    final value = await $settingsManager.getValue<bool>(kVisualizerLabKey);
    if (_disposed) return;
    _applyEnabled(value ?? false);
  }

  /// Follows the track on the player; the lab starts afresh on another one.
  void attachStatus(PlaybackStatusProvider status) {
    _status = status;
    final state = status.playbackStatus;
    final key = state.item ?? state.libPath;
    if (key == _trackKey) return;
    _trackKey = key;
    _plan = null;
    _planVersion = null;
    _songTime = null;
    _observed.clear();
    _temporary.clear();
    _watched.clear();
    if (_enabled) {
      SchedulerBinding.instance.addPostFrameCallback((_) {
        if (!_disposed) notifyListeners();
      });
    }
  }

  void setEnabled(bool enabled) {
    if (enabled == _enabled) return;
    $settingsManager.setValue(kVisualizerLabKey, enabled);
    _applyEnabled(enabled);
  }

  void _applyEnabled(bool enabled) {
    if (enabled == _enabled) return;
    _enabled = enabled;
    SetVisualizerLabRequest(enabled: enabled).sendSignalToRust();
    _lastEnableSent = DateTime.now();
    if (enabled) {
      _schedule(immediate: true);
    } else {
      _timer?.cancel();
      _timer = null;
      _panelOpen = false;
      _plan = null;
      _planVersion = null;
      _now = null;
      _songTime = null;
      _observed.clear();
      _temporary.clear();
      _watched.clear();
    }
    notifyListeners();
  }

  void setPanelOpen(bool open) {
    if (open == _panelOpen) return;
    _panelOpen = open;
    if (_enabled) _schedule(immediate: open);
    notifyListeners();
  }

  void togglePanel() => setPanelOpen(!_panelOpen);

  void _schedule({bool immediate = false}) {
    _timer?.cancel();
    _timer = null;
    if (!_enabled || _disposed) return;
    _timer = Timer(
      immediate
          ? Duration.zero
          : (_panelOpen ? _openInterval : _closedInterval),
      _poll,
    );
  }

  Future<void> _poll() async {
    _timer = null;
    if (!_enabled || _disposed || _polling) return;
    _polling = true;
    var changed = false;
    try {
      VisualizerLabRequest().sendSignalToRust();
      final response =
          (await VisualizerLabResponse.rustSignalStream.first.timeout(
            const Duration(seconds: 1),
          )).message;
      if (!_enabled || _disposed) return;
      _take(response);
      if (_planVersion != response.planVersion.toInt()) {
        changed = await _fetchPlan();
      }
    } on TimeoutException {
      // The next tick asks again.
    } finally {
      _polling = false;
      if (_enabled && !_disposed) {
        if (changed || _panelOpen) notifyListeners();
        if (_timer == null) _schedule();
      }
    }
  }

  void _take(VisualizerLabResponse response) {
    if (!response.inspecting) {
      final sent = _lastEnableSent;
      if (sent == null || DateTime.now().difference(sent) > _reEnableEvery) {
        SetVisualizerLabRequest(enabled: true).sendSignalToRust();
        _lastEnableSent = DateTime.now();
      }
    }

    var jumped = false;
    for (final event in response.events) {
      final mark = _markOf(event);
      if (mark == null) continue;
      if (mark.kind == LabMarkKind.jump) {
        _jumpTo(mark.time);
        jumped = true;
      }
      _observed.add(mark);
    }
    final time = response.songTime;
    final last = _songTime;
    // A seek back that no jump told of (the visualizer was not drawing).
    if (!jumped && last != null && time < last - _backwardsJump) {
      _jumpTo(time);
    }
    _watch(time);
    _songTime = time;
    _now = response.now;

    if (_observed.length > _maxMarks) {
      _observed.removeRange(0, _observed.length - _maxMarks);
    }
  }

  /// The visualizer went to song time [time] without playing up to it: what
  /// was seen after it is of another playing, and a new stretch starts there.
  void _jumpTo(double time) {
    _observed.removeWhere((m) => m.time > time);
    _temporary.removeWhere((m) => m.time > time);
    for (var i = _watched.length - 1; i >= 0; i--) {
      final (from, to) = _watched[i];
      if (from > time) {
        _watched.removeAt(i);
      } else if (to > time) {
        _watched[i] = (from, time);
      }
    }
    _watched.add((time, time));
  }

  /// The visualizer was seen playing at song time [time].
  void _watch(double time) {
    if (_watched.isEmpty || time < _watched.last.$1) {
      _watched.add((time, time));
      return;
    }
    final (from, to) = _watched.last;
    if (time > to) _watched[_watched.length - 1] = (from, time);
  }

  LabMark? _markOf(VisualizerLabEvent event) {
    final kind = event.kind;
    if (kind is VisualizerLabEventKindCut) {
      return LabMark(
        time: event.time,
        kind: kind.first ? LabMarkKind.jump : LabMarkKind.cut,
        size: kind.size,
        fill: kind.fill,
        strong: kind.strong,
        label: kind.shot,
      );
    } else if (kind is VisualizerLabEventKindLook) {
      return LabMark(
        time: event.time,
        kind: LabMarkKind.look,
        label: kind.look,
      );
    } else if (kind is VisualizerLabEventKindMoment) {
      return LabMark(
        time: event.time,
        kind: LabMarkKind.moment,
        strong: kind.strength == 'strong',
        filled: kind.effect != null,
        label: kind.effect ?? '',
      );
    }
    return null;
  }

  Future<bool> _fetchPlan() async {
    VisualizerPlanRequest().sendSignalToRust();
    try {
      final response =
          (await VisualizerPlanResponse.rustSignalStream.first.timeout(
            const Duration(seconds: 1),
          )).message;
      if (!_enabled || _disposed) return false;
      // None is a plan too: the song has no chart yet, or its plan is being
      // made. The version changes when one comes.
      _plan = response.plan;
      _planVersion = response.version.toInt();
      return true;
    } on TimeoutException {
      return false;
    }
  }

  /// Sends [command]. A command that plays something adds a temporary mark at
  /// the song position, named [markLabel] or after the command.
  void play(VisualizerLabCommand command, {String? markLabel}) {
    VisualizerLabCommandRequest(command: command).sendSignalToRust();
    final label = markLabel ?? _effectLabel(command);
    if (label == null) return;
    final time = _songTime ?? _status?.playbackStatus.progressSeconds ?? 0;
    _temporary.add(LabMark(time: time, kind: LabMarkKind.manual, label: label));
    notifyListeners();
  }

  static String? _effectLabel(VisualizerLabCommand command) {
    if (command is VisualizerLabCommandCut) return 'Cut';
    if (command is VisualizerLabCommandBurst) return 'Burst';
    if (command is VisualizerLabCommandFlare) return 'Flare';
    if (command is VisualizerLabCommandLeak) return 'Leak';
    if (command is VisualizerLabCommandLensGlitch) return 'Glitch';
    if (command is VisualizerLabCommandLensMosaic) return 'Mosaic';
    if (command is VisualizerLabCommandCutDressed) return 'Dressed cut';
    return null;
  }

  @override
  void dispose() {
    _disposed = true;
    _timer?.cancel();
    super.dispose();
  }
}
