import 'dart:math' as math;

import 'package:flutter/gestures.dart';
import 'package:provider/provider.dart';
import 'package:fluent_ui/fluent_ui.dart';

import '../../../bindings/bindings.dart';
import '../../../providers/visualizer_lab.dart';
import '../../../utils/format_time.dart';

// The progress bar's slider keeps this much of its width free on each side,
// which is where its thumb starts and ends.
const _sliderInset = 10.0;

const _rowHeight = 32.0;
const _rowGap = 4.0;
const _sectionsHeight = 22.0;

/// The height of the full timeline: the sections, the plan and what happened.
const labTimelineHeight = _sectionsHeight + 2 * (_rowHeight + _rowGap) - 4;

// A mark this many pixels from the pointer is described.
const _hoverReach = 5.0;

const _pink = Color(0xFFFF6FB5);
const _manual = Color(0xFFFFB02E);

Color labTierColor(String tier) => switch (tier) {
  'quiet' => const Color(0xFF4A78C2),
  'high' => const Color(0xFFD9573F),
  _ => const Color(0xFF3FA796),
};

// Taller for a wider shot.
double _cutHeight(String size) => switch (size) {
  'wide' => 20,
  'medium' => 16,
  'close' => 12,
  'detail' => 9,
  _ => 6,
};

/// Draws the plan of a song, or the marks of what happened, on a timeline.
/// The compact form draws only the section boundaries and the cuts, thin and
/// faint, for behind a progress bar.
class LabTimelinePainter extends CustomPainter {
  LabTimelinePainter({
    required this.plan,
    required this.duration,
    this.observed = const [],
    this.temporary = const [],
    this.songTime,
    this.compact = false,
    this.inset = 0,
    this.color = const Color(0xFFFFFFFF),
  });

  final VisualizerPlan plan;
  final double duration;
  final List<LabMark> observed;
  final List<LabMark> temporary;
  final double? songTime;
  final bool compact;
  final double inset;

  /// The color of cuts and text.
  final Color color;

  double _x(double time, Size size) {
    final width = size.width - 2 * inset;
    return inset + (duration <= 0 ? 0 : (time / duration).clamp(0, 1)) * width;
  }

  @override
  void paint(Canvas canvas, Size size) {
    if (compact) {
      _paintCompact(canvas, size);
      return;
    }
    var top = 0.0;
    _paintSections(canvas, size, top);
    top += _sectionsHeight + _rowGap;
    _paintRow(
      canvas,
      size,
      top,
      cuts: [
        for (final c in plan.cuts)
          LabMark(
            time: c.time,
            kind: LabMarkKind.cut,
            size: c.size,
            fill: c.fill,
            strong: c.strong,
          ),
      ],
      looks: [for (final l in plan.looks) l.time],
      moments: [
        for (final m in plan.moments)
          LabMark(
            time: m.time,
            kind: LabMarkKind.moment,
            strong: m.strength == 'strong',
            filled: m.effect != null,
          ),
      ],
    );
    top += _rowHeight + _rowGap;
    _paintRow(
      canvas,
      size,
      top,
      cuts: [
        for (final m in observed)
          if (m.kind == LabMarkKind.cut) m,
      ],
      looks: [
        for (final m in observed)
          if (m.kind == LabMarkKind.look) m.time,
      ],
      moments: [
        for (final m in observed)
          if (m.kind == LabMarkKind.moment) m,
      ],
    );

    final paint = Paint()
      ..color = _manual
      ..strokeWidth = 2;
    for (final m in temporary) {
      final x = _x(m.time, size);
      canvas.drawLine(Offset(x, top - 2), Offset(x, top + _rowHeight), paint);
      canvas.drawCircle(Offset(x, top - 1), 3, paint);
    }

    final now = songTime;
    if (now != null) {
      final x = _x(now, size);
      canvas.drawLine(
        Offset(x, 0),
        Offset(x, size.height),
        Paint()
          ..color = const Color(0xFFFFFFFF)
          ..strokeWidth = 1.5,
      );
    }
  }

  void _paintSections(Canvas canvas, Size size, double top) {
    for (final s in plan.sections) {
      final left = _x(s.start, size);
      final right = _x(s.end, size);
      if (right <= left) continue;
      final rect = Rect.fromLTRB(left, top, right - 1, top + _sectionsHeight);
      canvas.drawRRect(
        RRect.fromRectAndRadius(rect, const Radius.circular(2)),
        Paint()..color = labTierColor(s.tier).withAlpha(170),
      );
      final text = TextPainter(
        text: TextSpan(
          text: s.label,
          style: const TextStyle(fontSize: 10, color: Color(0xFFFFFFFF)),
        ),
        maxLines: 1,
        ellipsis: '…',
        textDirection: TextDirection.ltr,
      )..layout(maxWidth: math.max(0, rect.width - 4));
      if (text.width > 0 && rect.width > 12) {
        text.paint(
          canvas,
          Offset(rect.left + 3, rect.top + (_sectionsHeight - text.height) / 2),
        );
      }
    }
  }

  void _paintRow(
    Canvas canvas,
    Size size,
    double top, {
    required List<LabMark> cuts,
    required List<double> looks,
    required List<LabMark> moments,
  }) {
    final baseline = top + _rowHeight - 7;
    canvas.drawLine(
      Offset(inset, baseline),
      Offset(size.width - inset, baseline),
      Paint()
        ..color = color.withAlpha(40)
        ..strokeWidth = 1,
    );

    final tick = Paint()..color = color;
    final bracket = Paint()
      ..color = color.withAlpha(150)
      ..strokeWidth = 1
      ..style = PaintingStyle.stroke;

    double? burstStart;
    double? burstEnd;
    void closeBurst() {
      final a = burstStart;
      final b = burstEnd;
      if (a != null && b != null) {
        final y = baseline + 3;
        final path = Path()
          ..moveTo(_x(a, size), y - 2)
          ..lineTo(_x(a, size), y)
          ..lineTo(_x(b, size), y)
          ..lineTo(_x(b, size), y - 2);
        canvas.drawPath(path, bracket);
      }
      burstStart = null;
      burstEnd = null;
    }

    var lastFill = 0;
    for (final c in cuts) {
      if (c.fill > 0) {
        if (burstStart == null || c.fill <= lastFill) {
          closeBurst();
          burstStart = c.time;
        }
        burstEnd = c.time;
      } else {
        closeBurst();
      }
      lastFill = c.fill;
      final x = _x(c.time, size);
      tick.strokeWidth = c.strong ? 3 : 1.5;
      canvas.drawLine(
        Offset(x, baseline),
        Offset(x, baseline - _cutHeight(c.size)),
        tick,
      );
    }
    closeBurst();

    final pink = Paint()
      ..color = _pink
      ..strokeWidth = 2;
    for (final t in looks) {
      final x = _x(t, size);
      canvas.drawLine(Offset(x, top), Offset(x, top + 5), pink);
    }

    for (final m in moments) {
      final center = Offset(_x(m.time, size), baseline + 3.5);
      final radius = m.strong ? 3.0 : 2.2;
      if (m.filled) {
        canvas.drawCircle(center, radius, Paint()..color = color);
      } else {
        canvas.drawCircle(
          center,
          radius,
          Paint()
            ..color = color.withAlpha(170)
            ..style = PaintingStyle.stroke
            ..strokeWidth = 1,
        );
      }
    }
  }

  void _paintCompact(Canvas canvas, Size size) {
    final mid = size.height / 2;
    final boundary = Paint()
      ..color = color.withAlpha(70)
      ..strokeWidth = 1;
    for (final s in plan.sections) {
      if (s.start <= 0) continue;
      final x = _x(s.start, size);
      canvas.drawLine(Offset(x, mid - 8), Offset(x, mid + 8), boundary);
    }
    final tick = Paint()..color = color.withAlpha(110);
    for (final c in plan.cuts) {
      final x = _x(c.time, size);
      tick.strokeWidth = c.strong ? 2 : 1;
      canvas.drawLine(Offset(x, mid - 4), Offset(x, mid + 4), tick);
    }
  }

  @override
  bool shouldRepaint(LabTimelinePainter old) => true;
}

/// The timeline of the lab, which seeks where it is tapped or dragged.
class LabTimeline extends StatefulWidget {
  const LabTimeline({
    super.key,
    required this.plan,
    required this.observed,
    required this.temporary,
    required this.songTime,
    required this.onSeek,
  });

  final VisualizerPlan plan;
  final List<LabMark> observed;
  final List<LabMark> temporary;
  final double? songTime;

  /// Seeks to a song time, in seconds. `last` is true when the pointer lifts.
  final void Function(double seconds, bool last) onSeek;

  @override
  State<LabTimeline> createState() => _LabTimelineState();
}

class _LabTimelineState extends State<LabTimeline> {
  // Where a drag is, to seek there exactly when it ends.
  double? _dragTime;

  @override
  Widget build(BuildContext context) {
    final theme = FluentTheme.of(context);
    final plan = widget.plan;

    return LayoutBuilder(
      builder: (context, constraints) {
        final width = constraints.maxWidth;
        double timeAt(double dx) => (dx / width).clamp(0.0, 1.0) * plan.length;

        return MouseRegion(
          cursor: SystemMouseCursors.click,
          child: GestureDetector(
            behavior: HitTestBehavior.opaque,
            onTapDown: (d) => widget.onSeek(timeAt(d.localPosition.dx), true),
            onHorizontalDragUpdate: (d) {
              _dragTime = timeAt(d.localPosition.dx);
              widget.onSeek(_dragTime!, false);
            },
            onHorizontalDragEnd: (_) {
              final t = _dragTime;
              _dragTime = null;
              if (t != null) widget.onSeek(t, true);
            },
            child: _LabHover(
              describe: (local, _) => _describe(
                _rowAt(local.dy),
                plan,
                timeAt(local.dx),
                _hoverReach / width * plan.length,
                observed: widget.observed,
                temporary: widget.temporary,
              ),
              child: CustomPaint(
                size: Size(width, labTimelineHeight),
                painter: LabTimelinePainter(
                  plan: plan,
                  duration: plan.length,
                  observed: widget.observed,
                  temporary: widget.temporary,
                  songTime: widget.songTime,
                  color: theme.resources.textFillColorPrimary,
                ),
              ),
            ),
          ),
        );
      },
    );
  }
}

/// Puts the plan's section boundaries and cuts behind [slider], a progress
/// bar's slider, while the lab is on and has a plan. Otherwise it is [slider]
/// itself, so the tree is as it is without the lab.
Widget withLabMarkers(
  BuildContext context, {
  required Widget slider,
  required double duration,
}) {
  final plan = context.select<VisualizerLabProvider, VisualizerPlan?>(
    (p) => p.enabled ? p.plan : null,
  );
  if (plan == null) return slider;

  // The slider spans the player's duration, which the chart's length may miss
  // by a little.
  final length = duration > 0 ? duration : plan.length;
  final color = FluentTheme.of(context).resources.textFillColorPrimary;

  return _LabHover(
    describe: (local, size) {
      final width = math.max(1.0, size.width - 2 * _sliderInset);
      return _describe(
        _Row.compact,
        plan,
        ((local.dx - _sliderInset) / width).clamp(0.0, 1.0) * length,
        _hoverReach / width * length,
      );
    },
    child: Stack(
      alignment: Alignment.center,
      children: [
        Positioned.fill(
          child: IgnorePointer(
            child: CustomPaint(
              painter: LabTimelinePainter(
                plan: plan,
                duration: length,
                compact: true,
                inset: _sliderInset,
                color: color,
              ),
            ),
          ),
        ),
        slider,
      ],
    ),
  );
}

enum _Row { sections, plan, observed, compact }

// Which row of the full timeline a height is in.
_Row _rowAt(double y) {
  const planTop = _sectionsHeight + _rowGap;
  if (y < planTop - _rowGap / 2) return _Row.sections;
  if (y < planTop + _rowHeight + _rowGap / 2) return _Row.plan;
  return _Row.observed;
}

// The time with tenths of a second, as cuts are placed.
String _clock(double t) => '${formatTime(t)}.${(t * 10).floor() % 10}';

String _section(VisualizerPlanSection s) =>
    '${s.label} · ${s.tier} · ${formatTime(s.start)}–${formatTime(s.end)}';

String _cut(
  double time,
  String shot,
  String size, {
  int fill = 0,
  bool strong = false,
  bool first = false,
  bool glide = false,
  String? dress,
}) => [
  'Cut ${_clock(time)}',
  '$shot ($size)',
  if (first) 'first',
  if (fill > 0) 'burst $fill',
  if (strong) 'strong',
  if (glide) 'glide',
  ?dress,
].join(' · ');

String _moment(double time, bool strong, String? effect) =>
    'Moment ${_clock(time)} · ${strong ? 'strong' : 'medium'} · '
    '${effect ?? 'no effect'}';

/// What the marks of [row] within [reach] seconds of [time] are, nearest
/// first, one line each; on the sections row, the section at [time].
List<String> _describe(
  _Row row,
  VisualizerPlan plan,
  double time,
  double reach, {
  List<LabMark> observed = const [],
  List<LabMark> temporary = const [],
}) {
  final near = <(double, String)>[];
  void add(double at, String line) {
    final distance = (at - time).abs();
    if (distance <= reach) near.add((distance, line));
  }

  switch (row) {
    case _Row.sections:
      for (final s in plan.sections) {
        if (time >= s.start && time < s.end) return [_section(s)];
      }
      return const [];
    case _Row.compact:
      for (final s in plan.sections) {
        if (s.start > 0) add(s.start, 'Section ${_section(s)}');
      }
      for (final c in plan.cuts) {
        add(
          c.time,
          _cut(c.time, c.shot, c.size, fill: c.fill, strong: c.strong),
        );
      }
    case _Row.plan:
      for (final c in plan.cuts) {
        add(
          c.time,
          _cut(
            c.time,
            c.shot,
            c.size,
            fill: c.fill,
            strong: c.strong,
            first: c.first,
            glide: !c.hard,
            dress: c.dress,
          ),
        );
      }
      for (final l in plan.looks) {
        add(l.time, 'Look ${_clock(l.time)} · ${l.look}');
      }
      for (final m in plan.moments) {
        add(m.time, _moment(m.time, m.strength == 'strong', m.effect));
      }
    case _Row.observed:
      for (final m in observed) {
        add(m.time, switch (m.kind) {
          LabMarkKind.cut => _cut(
            m.time,
            m.label,
            m.size,
            fill: m.fill,
            strong: m.strong,
          ),
          LabMarkKind.look => 'Look ${_clock(m.time)} · ${m.label}',
          LabMarkKind.moment => _moment(
            m.time,
            m.strong,
            m.filled ? m.label : null,
          ),
          LabMarkKind.manual => 'By hand ${_clock(m.time)} · ${m.label}',
        });
      }
      for (final m in temporary) {
        add(m.time, 'By hand ${_clock(m.time)} · ${m.label}');
      }
  }
  near.sort((a, b) => a.$1.compareTo(b.$1));
  return [for (final (_, line) in near.take(4)) line];
}

/// Shows what is under the pointer in a label above [child], while the
/// pointer hovers it. [describe] gives the lines for a position in [child] of
/// a size; none hides the label.
class _LabHover extends StatefulWidget {
  const _LabHover({required this.describe, required this.child});

  final List<String> Function(Offset local, Size size) describe;
  final Widget child;

  @override
  State<_LabHover> createState() => _LabHoverState();
}

class _LabHoverState extends State<_LabHover> {
  final _portal = OverlayPortalController();
  Offset _pointer = Offset.zero;
  List<String> _lines = const [];

  void _hover(PointerHoverEvent event) {
    final box = context.findRenderObject() as RenderBox?;
    if (box == null || !box.hasSize) return;
    final lines = widget.describe(event.localPosition, box.size);
    setState(() {
      _pointer = event.localPosition;
      _lines = lines;
    });
    if (lines.isEmpty) {
      _portal.hide();
    } else {
      _portal.show();
    }
  }

  void _exit(PointerExitEvent event) => _portal.hide();

  @override
  Widget build(BuildContext context) {
    return OverlayPortal(
      controller: _portal,
      overlayChildBuilder: _label,
      child: MouseRegion(
        opaque: false,
        hitTestBehavior: HitTestBehavior.translucent,
        onHover: _hover,
        onExit: _exit,
        child: widget.child,
      ),
    );
  }

  Widget _label(BuildContext context) {
    final box = this.context.findRenderObject() as RenderBox?;
    final overlay =
        Overlay.of(this.context).context.findRenderObject() as RenderBox?;
    if (box == null || overlay == null || !box.attached || _lines.isEmpty) {
      return const SizedBox.shrink();
    }
    final topLeft = box.localToGlobal(Offset.zero, ancestor: overlay);

    return IgnorePointer(
      child: CustomSingleChildLayout(
        delegate: _LabelLayout(
          x: topLeft.dx + _pointer.dx,
          top: topLeft.dy,
          bottom: topLeft.dy + box.size.height,
        ),
        child: Container(
          constraints: const BoxConstraints(maxWidth: 360),
          padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 5),
          decoration: BoxDecoration(
            color: const Color(0xEE1C1C1C),
            borderRadius: BorderRadius.circular(4),
            border: Border.all(color: const Color(0x33FFFFFF)),
          ),
          child: Text(
            _lines.join('\n'),
            style: const TextStyle(
              fontSize: 11,
              height: 1.35,
              color: Color(0xFFFFFFFF),
            ),
          ),
        ),
      ),
    );
  }
}

// Centers the label over the pointer, above the hovered widget, or below it
// where there is no room above, and keeps it inside the overlay.
class _LabelLayout extends SingleChildLayoutDelegate {
  _LabelLayout({required this.x, required this.top, required this.bottom});

  final double x;
  final double top;
  final double bottom;

  @override
  BoxConstraints getConstraintsForChild(BoxConstraints constraints) =>
      constraints.loosen();

  @override
  Offset getPositionForChild(Size size, Size childSize) {
    final double left = (x - childSize.width / 2).clamp(
      4.0,
      math.max(4.0, size.width - childSize.width - 4),
    );
    var y = top - childSize.height - 6;
    if (y < 4) y = bottom + 6;
    return Offset(left, y);
  }

  @override
  bool shouldRelayout(_LabelLayout old) =>
      old.x != x || old.top != top || old.bottom != bottom;
}

/// How many of the planned cuts that are behind the song time have a cut that
/// happened within [tolerance] seconds of them, and how many have passed.
(int, int) labCutMatch(
  VisualizerPlan plan,
  List<LabMark> observed,
  double songTime, {
  double tolerance = 0.1,
}) {
  var passed = 0;
  var matched = 0;
  for (final c in plan.cuts) {
    if (c.time > songTime) break;
    passed++;
    for (final m in observed) {
      if (m.kind == LabMarkKind.cut && (m.time - c.time).abs() <= tolerance) {
        matched++;
        break;
      }
    }
  }
  return (matched, passed);
}
