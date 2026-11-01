import 'dart:math' as math;

import 'package:provider/provider.dart';
import 'package:fluent_ui/fluent_ui.dart';
import 'package:material_symbols_icons/symbols.dart';

import '../../../bindings/bindings.dart';
import '../../../providers/status.dart';
import '../../../providers/visualizer_lab.dart';
import '../../../utils/api/seek.dart';
import '../../../utils/format_time.dart';
import '../../settings/settings_block_title.dart';

import 'lab_timeline.dart';

/// The width the panel takes at most.
const visualizerLabPanelWidth = 360.0;

const _transitions = <(String, VisualizerLabTransition)>[
  ('Flash', VisualizerLabTransitionFlash()),
  ('Glitch', VisualizerLabTransitionGlitch()),
  ('Mosaic', VisualizerLabTransitionMosaic()),
  ('Fade', VisualizerLabTransitionFade()),
  ('Dissolve', VisualizerLabTransitionDissolve()),
];

/// The lab of the visualizer, docked on the left of the cover wall page: the
/// plan of the song next to what happened, live readouts and buttons that play
/// effects.
/// Labels are for developers and are not translated.
class VisualizerLabPanel extends StatelessWidget {
  const VisualizerLabPanel({super.key});

  @override
  Widget build(BuildContext context) {
    final outer = FluentTheme.of(context);
    // Over a scene, the panel is dark whatever the app's theme is.
    final theme = outer.brightness.isDark
        ? outer
        : FluentThemeData(
            brightness: Brightness.dark,
            accentColor: outer.accentColor,
            visualDensity: outer.visualDensity,
            fontFamily: outer.typography.body?.fontFamily,
          );

    return FluentTheme(
      data: theme,
      child: Builder(
        builder: (context) => DecoratedBox(
          decoration: BoxDecoration(
            color: const Color(0xFF000000).withAlpha(200),
            border: Border(right: BorderSide(color: const Color(0x22FFFFFF))),
          ),
          child: const _PanelBody(),
        ),
      ),
    );
  }
}

class _PanelBody extends StatelessWidget {
  const _PanelBody();

  @override
  Widget build(BuildContext context) {
    final theme = FluentTheme.of(context);
    final lab = context.watch<VisualizerLabProvider>();
    final now = lab.now;

    return SingleChildScrollView(
      padding: const EdgeInsets.fromLTRB(8, 8, 8, 12),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Row(
            children: [
              const SizedBox(width: 4),
              Expanded(
                child: Text(
                  'Visualizer Lab',
                  style: theme.typography.bodyStrong,
                ),
              ),
              IconButton(
                icon: const Icon(Symbols.close, size: 16),
                onPressed: () => lab.setPanelOpen(false),
              ),
            ],
          ),
          const SizedBox(height: 4),
          _Block(
            title: 'Timeline',
            subtitle: 'Planned and observed cuts',
            initiallyExpanded: true,
            child: _TimelineSection(lab: lab),
          ),
          _Block(
            title: 'Now',
            subtitle: 'Shot, look and level',
            initiallyExpanded: true,
            child: _NowSection(now: now),
          ),
          _Block(
            title: 'Signals',
            subtitle: 'Music signals and scene inputs',
            child: _SignalsSection(now: now),
          ),
          _Block(
            title: 'Play',
            subtitle: 'Effects, looks and shots',
            initiallyExpanded: true,
            child: _PlaySection(lab: lab, now: now),
          ),
          _Block(
            title: 'Overrides',
            subtitle: 'Replace what the visualizer chooses',
            child: _OverridesSection(lab: lab, now: now),
          ),
        ],
      ),
    );
  }
}

class _Block extends StatelessWidget {
  const _Block({
    required this.title,
    required this.subtitle,
    required this.child,
    this.initiallyExpanded = false,
  });

  final String title;
  final String subtitle;
  final Widget child;
  final bool initiallyExpanded;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 2),
      child: Expander(
        initiallyExpanded: initiallyExpanded,
        header: Padding(
          padding: const EdgeInsets.symmetric(vertical: 8),
          child: SettingsBlockTitle(title: title, subtitle: subtitle),
        ),
        content: SizedBox(width: double.infinity, child: child),
      ),
    );
  }
}

TextStyle? _caption(BuildContext context, {bool dim = false}) {
  final theme = FluentTheme.of(context);
  return theme.typography.caption?.apply(
    color: theme.resources.textFillColorPrimary.withAlpha(dim ? 110 : 230),
  );
}

class _Row extends StatelessWidget {
  const _Row(this.label, this.value);

  final String label;
  final String value;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 1.5),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: 64,
            child: Text(label, style: _caption(context, dim: true)),
          ),
          Expanded(child: Text(value, style: _caption(context))),
        ],
      ),
    );
  }
}

String _fixed(double value, [int digits = 1]) => value.toStringAsFixed(digits);

class _TimelineSection extends StatelessWidget {
  const _TimelineSection({required this.lab});

  final VisualizerLabProvider lab;

  @override
  Widget build(BuildContext context) {
    final plan = lab.plan;
    if (plan == null) {
      return Text(
        'No plan yet. The song needs a chart, and the plan takes a moment to make.',
        style: _caption(context, dim: true),
      );
    }

    final songTime = lab.songTime;
    final status = context.read<PlaybackStatusProvider>().playbackStatus;
    final (matched, passed) = songTime == null
        ? (0, 0)
        : labCutMatch(plan, lab.observed, songTime);

    // The first section that starts more than 4 s after a position that is
    // 4 s before it already.
    VisualizerPlanSection? next;
    if (songTime != null) {
      for (final s in plan.sections) {
        if (s.start - 4 > songTime + 0.25) {
          next = s;
          break;
        }
      }
    }

    void seekTo(double seconds, bool last) {
      if (last) {
        SeekRequest(positionSeconds: seconds).sendSignalToRust();
      } else if (status.duration > 0) {
        seek(seconds / status.duration * 100, status);
      }
    }

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        LabTimeline(
          plan: plan,
          observed: lab.observed,
          temporary: lab.temporary,
          songTime: songTime,
          onSeek: seekTo,
        ),
        const SizedBox(height: 8),
        Row(
          children: [
            Expanded(
              child: Text(
                'Match $matched / $passed'
                '${songTime == null ? '' : '   ${formatTime(songTime)}'}',
                style: _caption(context),
              ),
            ),
            Button(
              onPressed: next == null
                  ? null
                  : () => SeekRequest(
                      positionSeconds: math.max(0, next!.start - 4),
                    ).sendSignalToRust(),
              child: const Text('Next section'),
            ),
          ],
        ),
        const SizedBox(height: 4),
        Text(
          '${plan.scene}: ${plan.cuts.length} cuts, ${plan.moments.length} moments, '
          '${plan.looks.length} looks',
          style: _caption(context, dim: true),
        ),
      ],
    );
  }
}

class _NowSection extends StatelessWidget {
  const _NowSection({required this.now});

  final VisualizerLabNow? now;

  @override
  Widget build(BuildContext context) {
    final now = this.now;
    if (now == null) {
      return Text('Nothing drawn yet.', style: _caption(context, dim: true));
    }
    final director = now.director;
    final looks = now.looks;

    String lookName(int index) {
      if (looks == null || index >= looks.names.length) return '?';
      final name = looks.names[index];
      return name.isEmpty ? '#$index' : name;
    }

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        if (director == null)
          _Row('Shot', 'The scene has no shots.')
        else ...[
          _Row(
            'Shot',
            '${director.shotName.isEmpty ? '#${director.shotIndex}' : director.shotName}'
                '  ·  ${director.size}',
          ),
          _Row('Tier', director.tier),
          _Row(
            'Pace',
            director.pace == director.ownPace
                ? director.pace
                : '${director.pace} (own: ${director.ownPace})',
          ),
          _Row(
            'Age',
            '${_fixed(director.age)} / '
                '${director.length < 0 ? '?' : _fixed(director.length)} s',
          ),
          _Row('Held', director.held ? 'yes' : 'no'),
        ],
        const SizedBox(height: 4),
        if (looks == null)
          _Row('Look', 'The scene has no looks.')
        else ...[
          _Row('Look', lookName(looks.on)),
          _Row('Prev', lookName(looks.prev)),
          _Row('Mix', _fixed(looks.mix, 2)),
          _Row('Held', looks.held ? 'yes' : 'no'),
        ],
        const SizedBox(height: 4),
        _Row('Level', now.level),
      ],
    );
  }
}

class _SignalsSection extends StatelessWidget {
  const _SignalsSection({required this.now});

  final VisualizerLabNow? now;

  @override
  Widget build(BuildContext context) {
    final now = this.now;
    if (now == null) {
      return Text('Nothing drawn yet.', style: _caption(context, dim: true));
    }
    final theme = FluentTheme.of(context);
    final fg = theme.resources.textFillColorPrimary;

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        for (final signal in now.signals)
          Opacity(
            opacity: signal.meaningful ? 1 : 0.4,
            child: Padding(
              padding: const EdgeInsets.symmetric(vertical: 2),
              child: Row(
                children: [
                  SizedBox(
                    width: 104,
                    child: Text(
                      signal.name,
                      style: _caption(context),
                      overflow: TextOverflow.ellipsis,
                    ),
                  ),
                  Expanded(
                    child: ClipRRect(
                      borderRadius: BorderRadius.circular(2),
                      child: SizedBox(
                        height: 4,
                        child: Stack(
                          children: [
                            Positioned.fill(
                              child: ColoredBox(color: fg.withAlpha(30)),
                            ),
                            FractionallySizedBox(
                              widthFactor: signal.value.clamp(0.0, 1.0),
                              child: ColoredBox(color: theme.accentColor),
                            ),
                          ],
                        ),
                      ),
                    ),
                  ),
                  SizedBox(
                    width: 16,
                    child: signal.event
                        ? Center(
                            child: Container(
                              width: 8,
                              height: 8,
                              decoration: BoxDecoration(
                                shape: BoxShape.circle,
                                color: signal.value > 0.05
                                    ? theme.accentColor
                                    : fg.withAlpha(40),
                              ),
                            ),
                          )
                        : null,
                  ),
                ],
              ),
            ),
          ),
        if (now.inputs.isNotEmpty) ...[
          const SizedBox(height: 10),
          Text('Inputs', style: theme.typography.bodyStrong),
          const SizedBox(height: 4),
          for (final input in now.inputs)
            Padding(
              padding: const EdgeInsets.symmetric(vertical: 1.5),
              child: Row(
                children: [
                  Expanded(
                    child: Text(
                      input.label,
                      style: _caption(context, dim: true),
                      overflow: TextOverflow.ellipsis,
                    ),
                  ),
                  Text(_fixed(input.value, 2), style: _caption(context)),
                ],
              ),
            ),
        ],
      ],
    );
  }
}

class _PlaySection extends StatelessWidget {
  const _PlaySection({required this.lab, required this.now});

  final VisualizerLabProvider lab;
  final VisualizerLabNow? now;

  bool _declared(List<String>? names, String name) =>
      names?.any((n) => n.toLowerCase() == name) ?? false;

  Widget _button(
    String label,
    VisualizerLabCommand command, {
    bool declared = true,
  }) {
    return Opacity(
      opacity: declared ? 1 : 0.45,
      child: Button(onPressed: () => lab.play(command), child: Text(label)),
    );
  }

  @override
  Widget build(BuildContext context) {
    final theme = FluentTheme.of(context);
    final effects = now?.effects;
    final looks = now?.looks;
    final director = now?.director;

    final lens = effects?.lens;
    final cut = effects?.cut;

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Wrap(
          spacing: 6,
          runSpacing: 6,
          children: [
            _button(
              'Cut',
              const VisualizerLabCommandCut(),
              declared: director != null,
            ),
            _button(
              'Burst',
              const VisualizerLabCommandBurst(),
              declared: director != null,
            ),
            _button(
              'Flare',
              const VisualizerLabCommandFlare(),
              declared: _declared(lens, 'flare'),
            ),
            _button(
              'Leak',
              const VisualizerLabCommandLeak(),
              declared: _declared(lens, 'leak'),
            ),
            _button(
              'Glitch',
              const VisualizerLabCommandLensGlitch(),
              declared: _declared(lens, 'glitch'),
            ),
            _button(
              'Mosaic',
              const VisualizerLabCommandLensMosaic(),
              declared: _declared(lens, 'mosaic'),
            ),
          ],
        ),
        const SizedBox(height: 10),
        Text('Dressed cut', style: _caption(context, dim: true)),
        const SizedBox(height: 4),
        Wrap(
          spacing: 6,
          runSpacing: 6,
          children: [
            for (final (label, transition) in _transitions)
              _button(
                label,
                VisualizerLabCommandCutDressed(transition: transition),
                declared: _declared(cut, label.toLowerCase()),
              ),
          ],
        ),
        const SizedBox(height: 10),
        if (looks != null) ...[
          Text('Look', style: _caption(context, dim: true)),
          const SizedBox(height: 4),
          ComboBox<int>(
            isExpanded: true,
            value: looks.held ? looks.on : -1,
            items: [
              const ComboBoxItem(value: -1, child: Text('Auto')),
              for (var i = 0; i < looks.names.length; i++)
                ComboBoxItem(
                  value: i,
                  child: Text(looks.names[i].isEmpty ? '#$i' : looks.names[i]),
                ),
            ],
            onChanged: (value) {
              if (value == null) return;
              lab.play(
                VisualizerLabCommandLook(index: value < 0 ? null : value),
              );
            },
          ),
          const SizedBox(height: 10),
        ],
        if (director != null) ...[
          Text('Shot', style: _caption(context, dim: true)),
          const SizedBox(height: 4),
          ComboBox<int>(
            isExpanded: true,
            value: director.held ? director.shotIndex : -1,
            items: [
              const ComboBoxItem(value: -1, child: Text('Release')),
              for (var i = 0; i < director.shots.length; i++)
                ComboBoxItem(
                  value: i,
                  child: Text(
                    '${director.shots[i].name.isEmpty ? '#$i' : director.shots[i].name}'
                    ' (${director.shots[i].size})',
                  ),
                ),
            ],
            onChanged: (value) {
              if (value == null) return;
              lab.play(
                VisualizerLabCommandShot(index: value < 0 ? null : value),
                markLabel: value < 0 ? null : 'Shot',
              );
            },
          ),
        ],
        if (looks == null && director == null)
          Text(
            'The scene has no looks or shots.',
            style: theme.typography.caption,
          ),
      ],
    );
  }
}

class _OverridesSection extends StatefulWidget {
  const _OverridesSection({required this.lab, required this.now});

  final VisualizerLabProvider lab;
  final VisualizerLabNow? now;

  @override
  State<_OverridesSection> createState() => _OverridesSectionState();
}

class _OverridesSectionState extends State<_OverridesSection> {
  // The slider while it is dragged; none shows what the visualizer has.
  double? _dragChance;

  Widget _choices<T>({
    required String title,
    required T current,
    required List<(String, T)> options,
    required void Function(T) onSelect,
  }) {
    return Padding(
      padding: const EdgeInsets.only(bottom: 10),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(title, style: _caption(context, dim: true)),
          const SizedBox(height: 4),
          Wrap(
            spacing: 4,
            runSpacing: 4,
            children: [
              for (final (label, value) in options)
                ToggleButton(
                  checked: value == current,
                  onChanged: (_) => onSelect(value),
                  child: Text(label),
                ),
            ],
          ),
        ],
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final lab = widget.lab;
    final overrides = widget.now?.overrides;
    if (overrides == null) {
      return Text('Nothing drawn yet.', style: _caption(context, dim: true));
    }

    final chance = overrides.chance;
    final shown = _dragChance ?? (chance ?? 50).toDouble();

    final lens = overrides.lens;
    void setLens(VisualizerLabLens value) =>
        lab.play(VisualizerLabCommandLensWith(lens: value));

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          'Chance of an effect: ${chance == null ? 'setting' : '$chance %'}',
          style: _caption(context, dim: true),
        ),
        const SizedBox(height: 4),
        Row(
          children: [
            ToggleButton(
              checked: chance == null,
              onChanged: (_) {
                setState(() => _dragChance = null);
                lab.play(const VisualizerLabCommandEffectChance());
              },
              child: const Text('Setting'),
            ),
            const SizedBox(width: 8),
            Expanded(
              child: Slider(
                value: shown,
                min: 0,
                max: 100,
                divisions: 20,
                label: '${shown.round()}',
                onChanged: (v) => setState(() => _dragChance = v),
                onChangeEnd: (v) {
                  setState(() => _dragChance = null);
                  lab.play(
                    VisualizerLabCommandEffectChance(percent: v.round()),
                  );
                },
              ),
            ),
          ],
        ),
        const SizedBox(height: 10),
        _choices<bool?>(
          title: 'Bursts',
          current: overrides.bursts,
          options: const [('Auto', null), ('Always', true), ('Never', false)],
          onSelect: (v) => lab.play(VisualizerLabCommandBurstsWith(bursts: v)),
        ),
        _choices<VisualizerLabLevel?>(
          title: 'Level',
          current: overrides.level,
          options: const [
            ('Auto', null),
            ('A', VisualizerLabLevel.a),
            ('B', VisualizerLabLevel.b),
            ('C', VisualizerLabLevel.c),
          ],
          onSelect: (v) => lab.play(VisualizerLabCommandForceLevel(level: v)),
        ),
        _choices<String?>(
          title: 'Pace',
          current: overrides.pace,
          options: const [
            ('Own', null),
            ('Sections', 'sections'),
            ('Calm', 'calm'),
            ('Normal', 'normal'),
            ('Busy', 'busy'),
          ],
          onSelect: (v) => lab.play(
            VisualizerLabCommandPaceWith(
              pace: switch (v) {
                'sections' => VisualizerLabPace.sections,
                'calm' => VisualizerLabPace.calm,
                'normal' => VisualizerLabPace.normal,
                'busy' => VisualizerLabPace.busy,
                _ => null,
              },
            ),
          ),
        ),
        Text(
          'Forced lens at every moment',
          style: _caption(context, dim: true),
        ),
        const SizedBox(height: 4),
        Wrap(
          spacing: 4,
          runSpacing: 4,
          children: [
            ToggleButton(
              checked: lens.flare,
              onChanged: (v) => setLens(
                VisualizerLabLens(
                  flare: v,
                  leak: lens.leak,
                  glitch: lens.glitch,
                  mosaic: lens.mosaic,
                ),
              ),
              child: const Text('Flare'),
            ),
            ToggleButton(
              checked: lens.leak,
              onChanged: (v) => setLens(
                VisualizerLabLens(
                  flare: lens.flare,
                  leak: v,
                  glitch: lens.glitch,
                  mosaic: lens.mosaic,
                ),
              ),
              child: const Text('Leak'),
            ),
            ToggleButton(
              checked: lens.glitch,
              onChanged: (v) => setLens(
                VisualizerLabLens(
                  flare: lens.flare,
                  leak: lens.leak,
                  glitch: v,
                  mosaic: lens.mosaic,
                ),
              ),
              child: const Text('Glitch'),
            ),
            ToggleButton(
              checked: lens.mosaic,
              onChanged: (v) => setLens(
                VisualizerLabLens(
                  flare: lens.flare,
                  leak: lens.leak,
                  glitch: lens.glitch,
                  mosaic: v,
                ),
              ),
              child: const Text('Mosaic'),
            ),
          ],
        ),
      ],
    );
  }
}
