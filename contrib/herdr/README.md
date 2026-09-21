# Open clicked paths in sid from Herdr

This plugin opens files clicked in a Herdr terminal pane. It reuses sid in the
same tab, or elsewhere in the same workspace. If none is running, it opens sid
in a new pane to the right.

With Python 3.9+, Herdr and sid on PATH, run this from the sid checkout:

```sh
herdr plugin link "$PWD/contrib/herdr" --enabled
```

The plugin keeps the existing `hx-open` ID, so linking it replaces the old local
registration. Keep the checkout at this location while the plugin is linked.

Use Ctrl+click (or your configured Herdr link modifier) on a path such as
`pkg/errorx/format.go:509`. Relative paths resolve against the clicked pane's
working directory. The plugin also accepts `file:line:column`, `file(line,column)`
and local `file://` links, including percent-encoded spaces and `#L509` or
`#L509:3` positions. Paths that do not name an existing file are ignored.

Opening a file in an existing sid uses its default F10 command-line binding.
You can inspect failures with:

```sh
herdr plugin log list --plugin hx-open
```

From a shell, sid already accepts positions directly:

```sh
sid pkg/errorx/format.go:509
sid pkg/errorx/format.go:509:3
```
