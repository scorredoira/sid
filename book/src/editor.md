## Editor

- [`[editor]` Section](#editor-section)
- [`[editor.clipboard-provider]` Section](#editorclipboard-provider-section)
- [`[editor.statusline]` Section](#editorstatusline-section)
- [`[editor.lsp]` Section](#editorlsp-section)
- [`[editor.cursor-shape]` Section](#editorcursor-shape-section)
- [`[editor.file-picker]` Section](#editorfile-picker-section)
- [`[editor.file-explorer]` Section](#editorfile-explorer-section)
- [`[editor.sidebar]` Section](#editorsidebar-section)
- [`[editor.buffer-picker]` Section](#editorbuffer-picker-section)
- [`[editor.auto-pairs]` Section](#editorauto-pairs-section)
- [`[editor.auto-save]` Section](#editorauto-save-section)
- [`[editor.search]` Section](#editorsearch-section)
- [`[editor.whitespace]` Section](#editorwhitespace-section)
- [`[editor.indent-guides]` Section](#editorindent-guides-section)
- [`[editor.gutters]` Section](#editorgutters-section)
  - [`[editor.gutters.line-numbers]` Section](#editorguttersline-numbers-section)
  - [`[editor.gutters.diagnostics]` Section](#editorguttersdiagnostics-section)
  - [`[editor.gutters.diff]` Section](#editorguttersdiff-section)
  - [`[editor.gutters.spacer]` Section](#editorguttersspacer-section)
  - [`[editor.gutters.code-action-hint]` Section](#editorgutterscode-action-hint-section)
- [`[editor.soft-wrap]` Section](#editorsoft-wrap-section)
- [`[editor.smart-tab]` Section](#editorsmart-tab-section)
- [`[editor.inline-diagnostics]` Section](#editorinline-diagnostics-section)
- [`[editor.word-completion]` Section](#editorword-completion-section)
- [`[editor.workspace-trust]` Section](#editorworkspace-trust-section)

### `[editor]` Section

| Key | Description | Default |
|--|--|---------|
| `scrolloff` | Number of lines of padding around the edge of the screen when scrolling | `5` |
| `mouse` | Enable mouse mode. Dragging the line between two splits side by side, or the statusline of a split with another below it, resizes them | `true` |
| `mouse-yank-register` | Which register to use for mouse yanks. | `*` |
| `middle-click-paste` | Middle click paste support | `true` |
| `default-yank-register` | Default register used for yank/paste | `'"'` |
| `scroll-lines` | Number of lines to scroll per scroll wheel step | `3` |
| `shell` | Shell to use when running external commands | Unix: `["sh", "-c"]`<br/>Windows: `["cmd", "/C"]` |
| `line-number` | Line number display: `absolute` simply shows each line's number, while `relative` shows the distance from the current line. When unfocused or in insert mode, `relative` will still show absolute line numbers | `"absolute"` |
| `cursorline` | Highlight all lines with a cursor | `false` |
| `cursor-blink` | Whether the terminal's cursor blinks | `false` |
| `cursorcolumn` | Highlight all columns with a cursor | `false` |
| `continue-comments` | if helix should automatically add a line comment token if you create a new line inside a comment. | `true` |
| `gutters` | Gutters to display: Available are `diagnostics` and `diff` and `line-numbers` and `spacer` and `code-action-hint`, note that `diagnostics` also includes other features like breakpoints, 1-width padding will be inserted if gutters is non-empty | `["diagnostics", "spacer", "line-numbers", "spacer", "diff"]` |
| `auto-completion` | Enable automatic pop up of auto-completion | `true` |
| `path-completion` | Enable filepath completion. Show files and directories if an existing path at the cursor was recognized, either absolute or relative to the current opened document or current working directory (if the buffer is not yet saved). Defaults to true. | `true` |
| `auto-format` | Enable automatic formatting on save[^3] | `true` |
| `idle-timeout` | Time in milliseconds since last keypress before idle timers trigger. | `250` |
| `completion-timeout` | Time in milliseconds after typing a word character before completions are shown, set to 5 for instant.  | `250` |
| `preview-completion-insert` | Whether to apply completion item instantly when selected | `true` |
| `completion-trigger-len` | The min-length of word under cursor to trigger autocompletion | `2` |
| `completion-replace` | Whether to make completions always replace the entire word and not just the part before the cursor | `false` |
| `auto-info` | Whether to display info boxes | `true` |
| `true-color` | Whether to override automatic detection of terminal truecolor support in the event of a false negative | `false` |
| `undercurl` | Whether to override automatic detection of terminal undercurl support in the event of a false negative | `false` |
| `rulers` | List of column positions at which to display the rulers. Can be overridden by language specific `rulers` in `languages.toml` file | `[]` |
| `bufferline` | Renders a line at the top of the editor displaying open buffers. Can be `always`, `never` or `multiple` (only shown if more than one buffer is in use) | `"never"` |
| `color-modes` | Whether to color the mode indicator with different colors depending on the mode itself | `false` |
| `text-width` | Maximum line length. Used for the `:reflow` command and soft-wrapping if `soft-wrap.wrap-at-text-width` is set | `80` |
| `workspace-lsp-roots` | Directories relative to the workspace root that are treated as LSP roots. Should only be set in `.helix/config.toml` | `[]` |
| `default-line-ending` | The line ending to use for new documents. Can be `native`, `lf`, `crlf`, `ff`, `cr` or `nel`. `native` uses the platform's native line ending (`crlf` on Windows, otherwise `lf`). | `"native"` |
| `insert-final-newline` | Whether to automatically insert a trailing line-ending on write if missing | `true` |
| `atomic-save` | Whether to use atomic operations to write documents to disk. This prevents data loss if the editor is interrupted while writing the file, but may confuse some file watching/hot reloading programs. | `true` |
| `trim-final-newlines` | Whether to automatically remove line-endings after the final one on write | `false` |
| `trim-trailing-whitespace` | Whether to automatically remove whitespace preceding line endings on write | `false` |
| `popup-border` | Draw border around `popup`, `menu`, `all`, or `none` | `"none"` |
| `indent-heuristic` | How the indentation for a newly inserted line is computed: `simple` just copies the indentation level from the previous line, `tree-sitter` computes the indentation based on the syntax tree and `hybrid` combines both approaches. If the chosen heuristic is not available, a different one will be used as a fallback (the fallback order being `hybrid` -> `tree-sitter` -> `simple`). | `"hybrid"` |
| `jump-label-alphabet` | The characters that are used to generate two character jump labels. Characters at the start of the alphabet are used first. | `"abcdefghijklmnopqrstuvwxyz"` |
| `end-of-line-diagnostics` | Minimum severity of diagnostics to render at the end of the line. Set to `disable` to disable entirely. Refer to the setting about `inline-diagnostics` for more details | `"disable"` |
| `clipboard-provider` | Which API to use for clipboard interaction. One of `pasteboard` (MacOS), `wayland`, `x-clip`, `x-sel`, `win32-yank`, `termux`, `tmux`, `windows`, `termcode`, `none`, or a custom command set. | Platform and environment specific. |
| `editor-config` | Whether to read settings from [EditorConfig](https://editorconfig.org) files | `true` |
| `rainbow-brackets` | Whether to render rainbow colors for matching brackets. Requires tree-sitter `rainbows.scm` queries for the language. | `false` |
| `kitty-keyboard-protocol` | Whether to enable Kitty Keyboard Protocol. Can be `enabled`, `disabled` or `auto` | `"auto"` |

[^3]: In most cases, you also need to enable the `auto-format` setting under `languages.toml`. You can find the reasoning [here](https://github.com/helix-editor/helix/discussions/9043#discussioncomment-7811497).

### `[editor.clipboard-provider]` Section

Helix can be configured either to use a builtin clipboard configuration or to use
a provided command.

For instance, setting it to use OSC 52 termcodes, the configuration would be:
```toml
[editor]
clipboard-provider = "termcode"
```

Alternatively, Helix can be configured to use arbitrary commands for clipboard integration:

```toml
[editor.clipboard-provider.custom]
yank = { command = "cat",  args = ["test.txt"] }
paste = { command = "tee",  args = ["test.txt"] }
primary-yank = { command = "cat",  args = ["test-primary.txt"] } # optional
primary-paste = { command = "tee",  args = ["test-primary.txt"] } # optional
```

For custom commands the contents of the yank/paste is communicated over stdin/stdout.

### `[editor.statusline]` Section

Allows configuring the statusline at the bottom of the editor.

The configuration distinguishes between three areas of the status line:

`[ ... ... LEFT ... ... | ... ... ... CENTER ... ... ... | ... ... RIGHT ... ... ]`

Statusline elements can be defined as follows:

```toml
[editor.statusline]
left = ["mode", "spinner"]
center = ["file-name"]
right = ["diagnostics", "selections", "position", "file-encoding", "file-line-ending", "file-type"]
separator = "│"
mode.normal = "NORMAL"
mode.insert = "INSERT"
mode.select = "SELECT"
diagnostics = ["warning", "error"]
workspace-diagnostics = ["warning", "error"]
```
The `[editor.statusline]` key takes the following sub-keys:

| Key           | Description | Default |
| ---           | ---         | ---     |
| `left`        | A list of elements aligned to the left of the statusline | `["mode", "spinner", "file-name", "read-only-indicator", "file-modification-indicator"]` |
| `center`      | A list of elements aligned to the middle of the statusline | `[]` |
| `right`       | A list of elements aligned to the right of the statusline | `["diagnostics", "selections", "register", "position", "file-encoding"]` |
| `separator`   | The character used to separate elements in the statusline | `"│"` |
| `mode.normal` | The text shown in the `mode` element for normal mode | `"NOR"` |
| `mode.insert` | The text shown in the `mode` element for insert mode | `"INS"` |
| `mode.select` | The text shown in the `mode` element for select mode | `"SEL"` |
| `diagnostics` | A list of severities which are displayed for the current buffer | `["warning", "error"]` |
| `workspace-diagnostics` | A list of severities which are displayed for the workspace | `["warning", "error"]` |

The following statusline elements can be configured:

| Key    | Description |
| ------ | ----------- |
| `mode` | The current editor mode (`mode.normal`/`mode.insert`/`mode.select`) |
| `spinner` | A progress spinner indicating LSP activity |
| `file-name` | The path/name of the opened file |
| `file-absolute-path` | The absolute path/name of the opened file |
| `file-base-name` | The basename of the opened file |
| `current-working-directory` | The current working directory  |
| `file-modification-indicator` | The indicator to show whether the file is modified (a `[+]` appears when there are unsaved changes) |
| `file-encoding` | The encoding of the opened file if it differs from UTF-8 |
| `file-line-ending` | The file line endings (CRLF or LF) |
| `file-indent-style` | The file indentation style |
| `read-only-indicator` | An indicator that shows `[readonly]` when a file cannot be written |
| `total-line-numbers` | The total line numbers of the opened file |
| `file-type` | The type of the opened file |
| `diagnostics` | The number of warnings and/or errors |
| `workspace-diagnostics` | The number of warnings and/or errors on workspace |
| `selections` | The primary selection index out of the number of active selections |
| `primary-selection-length` | The number of characters currently in primary selection |
| `position` | The cursor position |
| `position-percentage` | The cursor position as a percentage of the total number of lines |
| `separator` | The string defined in `editor.statusline.separator` (defaults to `"│"`) |
| `spacer` | Inserts a space between elements (multiple/contiguous spacers may be specified) |
| `version-control` | The current branch name or detached commit hash of the opened workspace |
| `register` | The current selected register |
| `code-action-hint` | Indicator for when code actions are available |

### `[editor.lsp]` Section

| Key                   | Description                                                 | Default |
| ---                   | -----------                                                 | ------- |
| `enable`              | Enables LSP integration. Setting to false will completely disable language servers regardless of language settings.| `true` |
| `display-messages`    | Display LSP `window/showMessage` messages below statusline[^1] | `true` |
| `display-progress-messages` | Display LSP progress messages below statusline[^1]    | `false` |
| `auto-signature-help` | Enable automatic popup of signature help (parameter hints)  | `true`  |
| `auto-document-highlight` | Automatically highlight symbol references at the cursor | `false` |
| `display-inlay-hints` | Display inlay hints[^2]                                     | `false` |
| `inlay-hints-length-limit` | Maximum displayed length (non-zero number) of inlay hints | Unset by default  |
| `display-color-swatches` | Show color swatches next to colors | `true` |
| `display-signature-help-docs` | Display docs under signature help popup             | `true`  |
| `snippets`      | Enables snippet completions. Requires a server restart (`:lsp-restart`) to take effect after `:config-reload`/`:set`. | `true`  |
| `goto-reference-include-declaration` | Include declaration in the goto references popup. | `true`  |

[^1]: By default, a progress spinner is shown in the statusline beside the file path.

[^2]: You may also have to activate them in the language server config for them to appear, not just in Helix. Inlay hints in Helix are still being improved on and may be a little bit laggy/janky under some circumstances. Please report any bugs you see so we can fix them!

### `[editor.cursor-shape]` Section

Defines the shape of cursor in each mode.
Valid values for these options are `block`, `bar`, `underline`, or `hidden`.

> 💡 Due to limitations of the terminal environment, only the primary cursor can
> change shape.

| Key      | Description                                | Default   |
| ---      | -----------                                | -------   |
| `normal` | Cursor shape in [normal mode][normal mode] | `"block"` |
| `insert` | Cursor shape in [insert mode][insert mode] | `"block"` |
| `select` | Cursor shape in [select mode][select mode] | `"block"` |

[normal mode]: ./keymap.md#normal-mode
[insert mode]: ./keymap.md#insert-mode
[select mode]: ./keymap.md#select--extend-mode

### `[editor.file-picker]` Section

Set options for file picker and global search. Ignoring a file means it is
not visible in the Helix file picker and global search.

All git related options are only enabled in a git repository.

| Key | Description | Default |
|--|--|---------|
|`hidden` | Enables ignoring hidden files | `true`
|`follow-symlinks` | Follow symlinks instead of ignoring them | `true`
|`deduplicate-links` | Ignore symlinks that point at files already shown in the picker | `true`
|`parents` | Enables reading ignore files from parent directories | `true`
|`ignore` | Enables reading `.ignore` files | `true`
|`git-ignore` | Enables reading `.gitignore` files | `true`
|`git-global` | Enables reading global `.gitignore`, whose path is specified in git's config: `core.excludesfile` option | `true`
|`git-exclude` | Enables reading `.git/info/exclude` files | `true`
|`max-depth` | Set with an integer value for maximum depth to recurse | Unset by default

Ignore files can be placed locally as `.ignore` or put in your home directory as `~/.ignore`. They support the usual ignore and negative ignore (unignore) rules used in `.gitignore` files.

Additionally, you can use Helix-specific ignore files by creating a local `.helix/ignore` file in the current workspace or a global `ignore` file located in your Helix config directory:
- Linux and Mac: `~/.config/helix/ignore`
- Windows: `%AppData%\helix\ignore`

Example:

```ini
# unignore in file picker and global search
!.github/
!.gitignore
!.gitattributes
```

### `[editor.file-explorer]` Section

In addition to the options for the file picker and global search, a similar set of options is presented to configure the file explorer separately. However, unlike the file picker, the defaults are set to avoid ignoring most files.

Note that the ignore files consulted by the file explorer when `ignore` is set to true are the same ones used by the file picker, including the aforementioned Helix-specific ignore files.


| Key | Description | Default |
|--|--|---------|
|`hidden` | Enables ignoring hidden files | `false`
|`follow-symlinks` | Follow symlinks instead of ignoring them | `false`
|`parents` | Enables reading ignore files from parent directories | `false`
|`ignore` | Enables reading `.ignore` files | `false`
|`git-ignore` | Enables reading `.gitignore` files | `false`
|`git-global` | Enables reading global `.gitignore`, whose path is specified in git's config: `core.excludesfile` option | `false`
|`git-exclude` | Enables reading `.git/info/exclude` files | `false`
|`flatten-dirs` | Enables flattening single child directories | `true`

### `[editor.sidebar]` Section

The sidebar is the column on the left of the editor, the one `<space>T` focuses and `<space>t` shows or hides; its Files tab lists the workspace and follows the focused buffer, expanding its directory and marking the file. Inside it, `j`/`k` move, `l`/`Enter` expand a directory or open a file, `h` collapses or jumps to the parent, `H` collapses everything, `R` reloads, `Esc` returns to the editor and `q` hides the sidebar. `a` creates a file, or a directory when the name ends in `/`, next to the cursor; `r` renames the entry under the cursor, moving its open buffers along; `d` deletes it after confirmation. A click opens or expands the row under the mouse, and dragging the line that separates the sidebar from the editor resizes it; the width it is dragged to is remembered between sessions, over `width`. `Tab`, or a click on a tab's name, switches between three tabs: Files, the whole workspace; Changes, only what `git status` names — each file changed, added, deleted or renamed since the last commit, staged or not, with its letter (`M`, `A`, `D`, `R`) and a directory holding one directory folded into one row; and Commits, the history, newest first, each commit with its subject and its age, the older ones read as the cursor nears the end. Moving onto a commit, or a click on it, shows its whole diff in the focused view, in a scratch buffer; arriving at the tab leaves the view alone. `Enter` or a double click on a commit lists the files it touched the way Changes does, under a row standing for the whole commit, and the diff follows the cursor: the whole commit on its row, a directory's files on a directory, one file on a file. `Enter` on a file goes over to read that diff, and `Esc` goes back from the files to the history. `<space>H` turns Commits into the history of the current file, followed across renames, and `Enter` on one of its commits opens it with the cursor on that file; `Esc` goes back to the whole history. `<space>B` says in the status line who last changed the cursor's line, in which commit and when, counting the buffer's unsaved text; asked again on the same line, it opens that commit in the tree the same way. Changes and Commits ask git again every two seconds while they are on screen (a file's history only on `R`), and `R` asks at once; creating, renaming and deleting do nothing in Commits, which is the history and not the disk. What the Files tab lists follows the `[editor.file-explorer]` options and nothing else: the helix ignore files narrow what a search or a picker walks, never what the sidebar shows.

Commits uses one column, initially half the terminal width up to 80 columns. Drag its right edge to choose another width. Opening a commit shows its files below the history, initially in the bottom third; drag the horizontal `Files` divider to change that proportion. The width and height proportion are remembered between sessions. Each pane scrolls independently under the mouse, and `Alt-Up` / `Alt-Down` focus history / files from the sidebar. Very short terminals show the focused pane alone until both fit again. Right-click either panel for visibility controls: `F6` shows/hides commits and `F7` shows/hides code, preserving the open commit and selection. The visible panel fills the available space. Commit diffs are read-only code review buffers: filename headers, old/new line numbers, and code with language highlighting. Added and removed lines have tinted backgrounds; `⋯` separates omitted stretches. Git transport headers are hidden, while renames, binary changes, permissions and missing final newlines remain visible. Search, selection and copying work on the displayed text, without patch markers or line numbers. `F4`, or the code panel's context menu, toggles full file context: all unchanged lines of each changed text file become visible while additions and deletions stay marked. This reads the historical file in the commit, never the working tree, and keeps the current code location when switching. `Ctrl-Alt-d`, or the same menu, shows a diff side by side: the old side in the view and the new one in a view split to its right, each numbered by its own lines, with blank rows padding a change so each line faces the line it became. Both scroll, and move their cursor, together; neither wraps. `Ctrl-Alt-d` again shows one side above the other. Syntax is parsed from each side's excerpts or full context; excerpts lacking surrounding code may have less precise highlighting. Themes can set `ui.diff.header`, `ui.diff.added` and `ui.diff.removed`; by default the header uses `ui.statusline` and changes use tints derived from `diff.plus` and `diff.minus`.

`F1` opens a centered, read-only shortcut window built from the current keymaps, including custom sequences and macros. Type to filter, use `Tab` or the headings to filter by mode/sidebar, scroll with the wheel or arrow keys, and close with `Esc` or `F1`. No command runs from this reference.

| Key | Description | Default |
|--|--|---------|
| `open` | Whether the sidebar is shown when the editor starts on the project, with no file named on the command line; started on a file, it opens without the sidebar | `true` |
| `width` | Width of the sidebar in columns | `32` |

### `[editor.buffer-picker]` Section

Set options for buffer picker.

| Key | Description | Default |
|--|--|---------|
|`start-position` | Controls behavior for which buffer is initially selected | `current` |

Example

```toml
[editor.buffer-picker]
start-position = "previous"
```

### `[editor.auto-pairs]` Section

Enables automatic insertion of pairs to parentheses, brackets, etc. Can be a
simple boolean value, or a specific mapping of pairs of single characters.

To disable auto-pairs altogether, set `auto-pairs` to `false`:

```toml
[editor]
auto-pairs = false # defaults to `true`
```

The default pairs are <code>(){}[]''""``</code>, but these can be customized by
setting `auto-pairs` to a TOML table:

```toml
[editor.auto-pairs]
'(' = ')'
'{' = '}'
'[' = ']'
'"' = '"'
'`' = '`'
'<' = '>'
```

Additionally, this setting can be used in a language config. Unless
the editor setting is `false`, this will override the editor config in
documents with this language.

Example `languages.toml` that adds `<>` and removes `''`

```toml
[[language]]
name = "rust"

[language.auto-pairs]
'(' = ')'
'{' = '}'
'[' = ']'
'"' = '"'
'`' = '`'
'<' = '>'
```

### `[editor.auto-save]` Section

Control auto save behavior.

| Key | Description | Default |
|--|--|---------|
| `focus-lost` | Enable automatic saving on the focus moving away from Helix. Requires [focus event support](https://github.com/helix-editor/helix/wiki/Terminal-Support) from your terminal | `false` |
| `after-delay.enable` | Enable automatic saving after `auto-save.after-delay.timeout` milliseconds have passed since last edit. | `false` |
| `after-delay.timeout` | Time in milliseconds since last edit before auto save timer triggers. | `3000` |

### `[editor.search]` Section

Search specific options.

| Key | Description | Default |
|--|--|---------|
| `smart-case` | Enable smart case regex searching (case-insensitive unless pattern contains upper case characters) | `true` |
| `wrap-around`| Whether the search should wrap after depleting the matches | `true` |

### `[editor.whitespace]` Section

Options for rendering whitespace with visible characters. Use `:set whitespace.render all` to temporarily enable visible whitespace.

| Key | Description | Default |
|-----|-------------|---------|
| `render` | Whether to render whitespace. May either be `all` or `none`, or a table with sub-keys `space`, `nbsp`, `nnbsp`, `tab`, and `newline` | `"none"` |
| `characters` | Literal characters to use when rendering whitespace. Sub-keys may be any of `tab`, `space`, `nbsp`, `nnbsp`, `newline` or `tabpad` | See example below |

Example

```toml
[editor.whitespace]
render = "all"
# or control each character
[editor.whitespace.render]
space = "all"
tab = "all"
nbsp = "none"
nnbsp = "none"
newline = "none"

[editor.whitespace.characters]
space = "·"
nbsp = "⍽"
nnbsp = "␣"
tab = "→"
newline = "⏎"
tabpad = "·" # Tabs will look like "→···" (depending on tab width)
```

### `[editor.indent-guides]` Section

Options for rendering vertical indent guides.

| Key           | Description                                             | Default |
| ---           | ---                                                     | ---     |
| `render`      | Whether to render indent guides                         | `false` |
| `character`   | Literal character to use for rendering the indent guide | `"│"`   |
| `skip-levels` | Number of indent levels to skip                         | `0`     |

Example:

```toml
[editor.indent-guides]
render = true
character = "╎" # Some characters that work well: "▏", "┆", "┊", "⸽"
skip-levels = 1
```

### `[editor.gutters]` Section

For simplicity, `editor.gutters` accepts an array of gutter types, which will
use default settings for all gutter components.

```toml
[editor]
gutters = ["diff", "diagnostics", "line-numbers", "spacer"]
```

To customize the behavior of gutters, the `[editor.gutters]` section must
be used. This section contains top level settings, as well as settings for
specific gutter components as subsections.

| Key      | Description                    | Default                                                       |
| ---      | ---                            | ---                                                           |
| `layout` | A vector of gutters to display | `["diagnostics", "spacer", "line-numbers", "spacer", "diff"]` |

Example:

```toml
[editor.gutters]
layout = ["diff", "diagnostics", "line-numbers", "spacer"]
```

#### `[editor.gutters.line-numbers]` Section

Options for the line number gutter

| Key         | Description                             | Default |
| ---         | ---                                     | ---     |
| `min-width` | The minimum number of characters to use | `3`     |

Example:

```toml
[editor.gutters.line-numbers]
min-width = 1
```

#### `[editor.gutters.diagnostics]` Section

Currently unused

#### `[editor.gutters.diff]` Section

The `diff` gutter option displays colored bars indicating whether a `git` diff represents that a line was added, removed or changed.
These colors are controlled by the theme attributes `diff.plus`, `diff.minus` and `diff.delta`.

Other diff providers will eventually be supported by a future plugin system.

There are currently no options for this section.

#### `[editor.gutters.spacer]` Section

Currently unused

#### `[editor.gutters.code-action-hint]` Section

The `code-action-hint` gutter option displays an indicator for whether a code action is available at current selection.

There are currently no options for this section.

### `[editor.soft-wrap]` Section

Options for soft wrapping lines that exceed the view width:

| Key                  | Description                                                  | Default |
| ---                  | ---                                                          | ---     |
| `enable`             | Whether soft wrapping is enabled.                            | `false` |
| `max-wrap`           | Maximum free space left at the end of the line.              | `20`    |
| `max-indent-retain`  | Maximum indentation to carry over when soft wrapping a line. | `40`    |
| `wrap-indicator`     | Text inserted before soft wrapped lines, highlighted with `ui.virtual.wrap` | `"↪ "`    |
| `wrap-at-text-width` | Soft wrap at `text-width` instead of using the full viewport size. | `false` |

Example:

```toml
[editor.soft-wrap]
enable = true
max-wrap = 25 # increase value to reduce forced mid-word wrapping
max-indent-retain = 0
wrap-indicator = ""  # set wrap-indicator to "" to hide it
```

### `[editor.smart-tab]` Section

Options for navigating and editing using tab key.

| Key        | Description | Default |
|------------|-------------|---------|
| `enable` | If set to true, then when the cursor is in a position with non-whitespace to its left, instead of inserting a tab, it will run `move_parent_node_end`. If there is only whitespace to the left, then it inserts a tab as normal. With the default bindings, to explicitly insert a tab character, press Shift-tab. | `true` |
| `supersede-menu` | Normally, when a menu is on screen, such as when auto complete is triggered, the tab key is bound to cycling through the items. This means when menus are on screen, one cannot use the tab key to trigger the `smart-tab` command. If this option is set to true, the `smart-tab` command always takes precedence, which means one cannot use the tab key to cycle through menu items. One of the other bindings must be used instead, such as arrow keys or `C-n`/`C-p`. | `false` |


Due to lack of support for S-tab in some terminals, the default keybindings don't fully embrace smart-tab editing experience. If you enjoy smart-tab navigation and a terminal that supports the [Enhanced Keyboard protocol](https://github.com/helix-editor/helix/wiki/Terminal-Support#enhanced-keyboard-protocol), consider setting extra keybindings:

```toml
[keys.normal]
tab = "move_parent_node_end"
S-tab = "move_parent_node_start"

[keys.insert]
S-tab = "move_parent_node_start"

[keys.select]
tab = "extend_parent_node_end"
S-tab = "extend_parent_node_start"
```

### `[editor.inline-diagnostics]` Section

Off by default: what a language server has to say about the line the cursor is on is
drawn in a box floating over the code below it, so that nothing in the file moves when a
diagnostic appears or goes away. Turning these options on writes the message between the
lines instead, as shown below, which pushes the rest of the file down.

Options for rendering diagnostics inside the text like shown below

```text
fn main() {
  let foo = bar;
            └─ no such value in this scope
}
```

| Key        | Description | Default |
|------------|-------------|---------|
| `cursor-line` | The minimum severity that a diagnostic must have to be shown inline on the line that contains the primary cursor. Set to `disable` to not show any diagnostics inline. This option does not have any effect when in insert-mode and will only take effect 350ms after moving the cursor to a different line. | `"disable"` |
| `other-lines` | The minimum severity that a diagnostic must have to be shown inline on a line that does not contain the cursor-line. Set to `disable` to not show any diagnostics inline. | `"disable"` |
| `prefix-len` | How many horizontal bars `─` are rendered before the diagnostic text.  | `1` |
| `max-wrap` | Equivalent of the `editor.soft-wrap.max-wrap` option for diagnostics.  | `20` |
| `max-diagnostics` | Maximum number of diagnostics to render inline for a given line  | `10` |

The allowed values for `cursor-line` and `other-lines` are: `error`, `warning`, `info`, `hint`.

The (first) diagnostic with the highest severity that is not shown inline is rendered at the end of the line (as long as its severity is higher than the `end-of-line-diagnostics` config option):

```text
fn main() {
  let baz = 1;
  let foo = bar; a local variable with a similar name exists: baz
            └─ no such value in this scope
}
```

### `[editor.word-completion]` Section

Options for controlling completion of words from open buffers.

| Key                  | Description                                                    | Default  |
| ---                  | ---                                                            | ---      |
| `enable`             | Whether word completion is enabled                             | `true`   |
| `trigger-length`     | Number of word characters to type before triggering completion | `7`      |

Example:

```toml
[editor.word-completion]
enable = true
# Set the trigger length lower so that words are completed more often
trigger-length = 4
```

### `[editor.workspace-trust]` Section

Controls implicit workspace trust. See the [workspace
trust](./workspace-trust.md) chapter for the full feature.

| Key       | Description                                                              | Default     |
| ---       | ---                                                                      | ---         |
| `level`   | The default level of trust for every workspace.                         | `"servers"` |
| `prompt`  | Whether to show a modal when opening a file in an untrusted workspace.   | `true`      |
| `trusted` | Glob patterns whose matching workspaces are trusted without a grant.     | `[]`        |

Example:

```toml
[editor.workspace-trust]
# Even if `false`, the statusline `[⚠]` indicator is still shown.
prompt = false

# "none":     prompt for every workspace.
# "servers":  trust LSP and DAP launches but still gate local config and git;
#             .helix/config.toml, .helix/languages.toml, etc. need :workspace-trust.
# "insecure": trust everything (discouraged).
level = "servers"

# Discouraged: skips .helix/ change detection and trusts anything that lands
# under a matching path. `~` and environment variables are expanded.
trusted = ["~/src/github.com/me/*"]
```
