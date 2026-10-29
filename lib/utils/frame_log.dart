import 'dart:io';
import 'dart:ui';

import 'package:flutter/scheduler.dart';

/// Experiment: with RUNE_FRAME_LOG=1, append frame timings every 5 s to
/// <temp>/rune-frames.log (frames, and median / p90 / max of build, raster and
/// total span in ms).
void startFrameLog() {
  if (Platform.environment['RUNE_FRAME_LOG'] != '1') return;
  final file = File('${Directory.systemTemp.path}/rune-frames.log');
  final timings = <FrameTiming>[];
  var last = DateTime.now();
  SchedulerBinding.instance.addTimingsCallback((list) {
    timings.addAll(list);
    final now = DateTime.now();
    if (now.difference(last).inSeconds < 5) return;
    String stats(Iterable<Duration> ds) {
      final ms = ds.map((d) => d.inMicroseconds / 1000).toList()..sort();
      String at(double q) => ms[((ms.length - 1) * q).round()].toStringAsFixed(1);
      return '${at(0.5)}/${at(0.9)}/${ms.last.toStringAsFixed(1)}';
    }

    final secs = now.difference(last).inMilliseconds / 1000;
    file.writeAsStringSync(
      '${now.toIso8601String()} fps ${(timings.length / secs).toStringAsFixed(1)}'
      ' build ${stats(timings.map((t) => t.buildDuration))}'
      ' raster ${stats(timings.map((t) => t.rasterDuration))}'
      ' total ${stats(timings.map((t) => t.totalSpan))}\n',
      mode: FileMode.append,
    );
    timings.clear();
    last = now;
  });
}
