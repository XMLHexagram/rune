import 'dart:collection';

import 'package:fluent_ui/fluent_ui.dart';
import 'package:go_router/go_router.dart';
import 'package:player/widgets/flip_animation.dart';

class NavigationItem {
  final String title;
  final String path;
  final bool hidden;
  final List<NavigationItem>? children;

  NavigationItem(this.title, this.path,
      {this.hidden = false, this.children = const []});

  @override
  String toString() {
    return 'NavigationItem(title: $title, path: $path, hidden: $hidden)';
  }
}

class NavigationQuery {
  final HashMap<String, NavigationItem> _pathToItem = HashMap();
  final HashMap<String, String> _pathToParent = HashMap();
  final HashMap<String, List<NavigationItem>> _pathToChildren = HashMap();
  final List<NavigationItem> _rootItems = [];

  NavigationQuery(List<NavigationItem> items) {
    for (var item in items) {
      _addItem(item, null);
    }
  }

  void _addItem(NavigationItem item, String? parentPath) {
    if (item.hidden) return;

    _pathToItem[item.path] = item;
    if (parentPath != null) {
      _pathToParent[item.path] = parentPath;
      if (!_pathToChildren.containsKey(parentPath)) {
        _pathToChildren[parentPath] = [];
      }
      _pathToChildren[parentPath]!.add(item);
    } else {
      _rootItems.add(item);
    }

    final children = item.children;
    if (children != null) {
      for (var child in children) {
        _addItem(child, item.path);
      }
    }
  }

  NavigationItem? getItem(String? path) {
    return _pathToItem[path];
  }

  NavigationItem? getParent(String? path) {
    var parentPath = _pathToParent[path];
    if (parentPath != null) {
      return _pathToItem[parentPath];
    }
    return null;
  }

  List<NavigationItem>? getChildren(String? path) {
    return _pathToChildren[path];
  }

  List<NavigationItem>? getSiblings(String path) {
    var parentPath = _pathToParent[path];
    if (parentPath == null) {
      // If there's no parent, it means this item is a root item
      // Its siblings are all other root items
      return _rootItems.toList();
    }

    var siblings = _pathToChildren[parentPath];
    if (siblings == null) {
      return null;
    }

    // Filter out the item itself from the list of siblings
    return siblings.toList();
  }
}

class NavigationBar extends StatefulWidget {
  final List<NavigationItem> items;
  final String defaultPath;
  late final NavigationQuery query;

  NavigationBar({super.key, required this.items, this.defaultPath = "/"}) {
    query = NavigationQuery(items);
  }

  @override
  NavigationBarState createState() => NavigationBarState();
}

class NavigationBarState extends State<NavigationBar> {
  void _onRouteSelected(NavigationItem route) {
    GoRouter.of(context).push(route.path);
  }

  void _onBack(BuildContext context) {
    final path = GoRouterState.of(context).fullPath;

    setState(() {
      if (context.canPop() == false) {
        final parent = widget.query.getParent(path ?? widget.defaultPath);

        if (parent != null) {
          context.go(parent.path);
        }
      } else {
        context.pop();
      }

      setState(() {});
    });
  }

  @override
  Widget build(BuildContext context) {
    final path = GoRouterState.of(context).fullPath;
    final item = widget.query.getItem(path);
    final children =
        widget.query.getChildren(path)?.where((x) => !x.hidden).toList();
    final parent = widget.query.getParent(path ?? widget.defaultPath);
    final slibings = widget.query.getSiblings(item?.path ?? widget.defaultPath);

    print(slibings);

    final parentWidget = Row(
      mainAxisAlignment: MainAxisAlignment.start,
      children: slibings?.map((route) {
            return Padding(
              padding: const EdgeInsets.only(right: 12),
              child: GestureDetector(
                  onTap: () {
                    _onBack(context);
                    final flipAnimation = FlipAnimationManager.of(context);
                    final id = item?.path ?? parent?.path;

                    WidgetsBinding.instance.addPostFrameCallback((_) {
                      flipAnimation?.flipAnimation('title:$id', 'child:$id');
                    });
                  },
                  child: FlipText(
                      flipKey: 'title:${route.path}', text: route.title)),
            );
          }).toList() ??
          [],
    );

    final childrenWidget = Row(
      mainAxisAlignment: MainAxisAlignment.start,
      children: children?.map((route) {
            return Padding(
              padding: const EdgeInsets.only(right: 12),
              child: GestureDetector(
                  onTap: () {
                    _onRouteSelected(route);
                    final flipAnimation = FlipAnimationManager.of(context);
                    final id = route.path;

                    WidgetsBinding.instance.addPostFrameCallback((_) {
                      flipAnimation?.flipAnimation('child:$id', 'title:$id');
                    });
                  },
                  child: FlipText(
                      flipKey: 'child:${route.path}', text: route.title)),
            );
          }).toList() ??
          [],
    );

    return parent == null
        ? Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [childrenWidget])
        : Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              parentWidget,
              childrenWidget,
            ],
          );
  }
}
