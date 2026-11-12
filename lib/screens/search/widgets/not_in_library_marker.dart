import 'package:fluent_ui/fluent_ui.dart';
import 'package:material_symbols_icons/symbols.dart';

import '../../../utils/l10n.dart';

/// A small icon with a tooltip that tells a search result is not in the
/// library yet.
class NotInLibraryMarker extends StatelessWidget {
  const NotInLibraryMarker({super.key});

  @override
  Widget build(BuildContext context) {
    final color = FluentTheme.of(context).resources.textFillColorSecondary;
    return Tooltip(
      message: S.of(context).notInYourLibrary,
      child: Icon(Symbols.cloud, size: 14, color: color),
    );
  }
}

/// A title with the [NotInLibraryMarker] after it when [outside] is true.
class SearchItemTitle extends StatelessWidget {
  final String title;
  final bool outside;
  final TextStyle? style;

  const SearchItemTitle({
    super.key,
    required this.title,
    required this.outside,
    this.style,
  });

  @override
  Widget build(BuildContext context) {
    final text = Text(title, style: style, overflow: TextOverflow.ellipsis);
    if (!outside) return text;
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Flexible(child: text),
        const SizedBox(width: 6),
        const NotInLibraryMarker(),
      ],
    );
  }
}
