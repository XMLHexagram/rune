import 'package:fluent_ui/fluent_ui.dart';
import 'package:player/widgets/tile/config.dart';
import 'package:player/widgets/tile/fancy_cover_implementation.dart';

class FancyCover extends StatelessWidget {
  final double size;
  final (String, String, String) texts;

  const FancyCover({
    super.key,
    required this.size,
    required this.texts,
  });

  @override
  Widget build(BuildContext context) {
    final theme = FluentTheme.of(context);
    final c = theme.accentColor;
    final h = texts.hashCode;

    final group1 = [c.light, c.lighter, c.lightest];
    final group2 = [c.dark, c.darker, c.darkest];
    final foreground = h % 2 == 0 ? group1 : group2;
    final background = h % 2 == 1 ? group1 : group2;

    final i = h % 3;

    return ClipRRect(
      child: FancyCoverImplementation(
        size: size,
        texts: [texts.$1, texts.$2, texts.$3],
        configs: configs[texts.hashCode % configs.length],
        background: background[i],
        foreground: foreground[i],
      ),
    );
  }
}