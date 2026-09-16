#!/usr/bin/env bash
# Takes the screenshots in fork/screenshots from the built editor
# (target/release/sid), driving it inside tmux over a throwaway clone of this
# repository, so each picture shows the fork exactly as it is.
#
# Needs tmux, git and python3 with Pillow. Build first: cargo build --release
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(dirname "$here")
sid="$repo/target/release/sid"
out="$here/screenshots"
socket=fork-screenshots
work=$(mktemp -d)
trap 'tmux -L "$socket" kill-server 2>/dev/null || true; rm -rf "$work"' EXIT

if [[ ! -x "$sid" ]]; then
	echo "$sid is not built: cargo build --release" >&2
	exit 1
fi

# A partial clone fetches the blobs a followed history needs only when asked, and
# the shared clone below cannot ask: ask here, where it can.
git -C "$repo" log --follow --format=%h -- helix-term/src/ui/picker.rs >/dev/null
git clone --quiet --shared "$repo" "$work/demo"
mkdir -p "$work/data" "$work/config/sid" "$out"
cp "$here/demo-config.toml" "$work/config/sid/config.toml"
# The sidebar as wide as a drag of its separator would leave it, remembered.
mkdir -p "$work/data/sid"
echo 'width = 40' >"$work/data/sid/sidebar.toml"

demo="$work/demo"

# Starts the editor on the given files, alone on a fresh screen.
start() {
	tmux -L "$socket" kill-server 2>/dev/null || true
	sleep 0.3
	tmux -L "$socket" -f /dev/null new-session -d -s shot -x 132 -y 36 -c "$demo" \
		"env XDG_DATA_HOME=$work/data XDG_CONFIG_HOME=$work/config \
		SID_RUNTIME=$repo/runtime COLORTERM=truecolor $sid $*; sleep 600"
	sleep 2
}

# Sends keys as tmux names them (Space, Enter, M-h...).
keys() {
	tmux -L "$socket" send-keys -t shot "$@"
	sleep 0.4
}

# Types text as it is.
typed() {
	tmux -L "$socket" send-keys -t shot -l "$1"
	sleep 0.4
}

# Captures the screen, once whatever it waits on (git, the search) has answered.
shoot() {
	sleep "${2:-1.5}"
	tmux -L "$socket" capture-pane -t shot -p -e -N >"$work/$1.ansi"
	python3 "$here/render.py" "$work/$1.ansi" "$out/$1.png"
	echo "$out/$1.png"
}

# The position of a commit in the history list, found by its subject.
commit_row() {
	git -C "$demo" log --format=%s | grep -n -m1 -F "$1" | cut -d: -f1
}

# Moves the tree's cursor down n rows.
down() {
	for ((i = 1; i < $1; i++)); do
		tmux -L "$socket" send-keys -t shot Down
	done
	sleep 0.4
}

# The stacked history/files layout and the searchable shortcut popup.
review_shots() {
	start helix-term/src/ui/sidebar/mod.rs
	keys F6 Home
	down "$(commit_row "ui: show commit files in a side column")"
	keys Enter
	shoot commits 2
	keys Escape
	keys S-F1
	typed review_
	shoot shortcuts
	keys Escape
	# The same diff side by side, with the code panel alone on the screen.
	keys C-M-d
	keys F6
	shoot side-by-side 2
}

# Nothing open yet: what the editor shows before there is a file to show.
welcome_shot() {
	start
	shoot welcome
}

if [[ "${1:-}" == "--welcome-only" ]]; then
	welcome_shot
	exit 0
fi

if [[ "${1:-}" == "--review-only" ]]; then
	review_shots
	exit 0
fi

# First, while no run has left a session behind to reopen.
welcome_shot

start helix-term/src/ui/editor.rs helix-term/src/ui/sidebar/commits.rs
keys Space T
shoot tree

review_shots

start helix-term/src/ui/sidebar/commits.rs
keys 3 9 G
keys Space B
shoot blame 2

start helix-term/src/ui/editor.rs
keys Space Space
typed set_status
keys M-h M-i
keys Tab
typed set_message
keys Tab
typed 'helix-term/**'
shoot search 2

start helix-term/src/ui/sidebar/list.rs
keys C-f
typed cursor
keys M-h
keys Tab
typed selected
shoot search-file 2

# The symbols of a file, from its syntax tree: no language server to wait on.
start helix-term/src/ui/editor.rs
keys Space s
typed render
shoot symbols 2

# Neither Ctrl-Shift-b nor Ctrl-Shift-m crosses tmux, so the preview is asked for by name.
start README.md
keys Space '?'
typed markdown_preview_toggle
keys Enter
keys C-g
typed "$(grep -n -m1 '^## Ready as installed' "$demo/README.md" | cut -d: -f1)"
keys Enter
keys z t
shoot preview

# The same file with the preview on its own, a tab of its own beside it, asked for by
# name for the same reason. Two files open, so the bufferline is there to show the tab.
start README.md helix-term/src/ui/editor.rs
keys C-g
typed "$(grep -n -m1 '^## Ready as installed' "$demo/README.md" | cut -d: -f1)"
keys Enter
keys z t
keys Space '?'
typed markdown_preview_full
keys Enter
shoot preview-full

# The settings, opened by name so the picture does not depend on the terminal
# forwarding Ctrl and a comma.
start README.md
keys Space '?'
typed settings
keys Enter
keys Down Down Down
shoot settings

# What cannot be saved for you is asked about, in the middle of the screen.
start README.md
keys C-n
keys i
typed "notes I never gave a file to"
keys Escape
keys C-q
shoot quit

# And the question that takes a name, rather than refusing to write the buffer.
start README.md
keys C-n
keys i
typed "notes I never gave a file to"
keys Escape
keys C-s
shoot save-as

start --vsplit helix-term/src/ui/sidebar/mod.rs helix-term/src/ui/sidebar/tab.rs
keys Space T
keys Escape
shoot splits

# The document menu with both split directions and the way back to one pane.
start --vsplit helix-term/src/ui/sidebar/mod.rs helix-term/src/ui/sidebar/tab.rs
# SGR mouse: right button down at column 80, row 10 (one-based).
tmux -L "$socket" send-keys -t shot -H 1b 5b 3c 32 3b 38 30 3b 31 30 4d
keys Down Down Down Down Down Down Down
shoot split-menu

start helix-term/src/ui/picker.rs
keys Space H
shoot history 2

# The tree narrowed to the files whose path has "pick" in it, wherever they are.
start helix-term/src/ui/editor.rs
keys Space T
keys C-f
typed pick
shoot filter 2

# A working tree with something to show: a file changed, one added, one gone.
echo "// a line nobody has committed" >>"$demo/helix-term/src/ui/sidebar/list.rs"
echo "notes" >"$demo/NOTES.md"
git -C "$demo" add NOTES.md
git -C "$demo" rm --quiet helix-term/src/ui/sidebar/tab.rs
start + helix-term/src/ui/sidebar/list.rs
keys Space T
keys Tab
# The file the cursor is on shows what changed in it.
keys Enter
shoot changes 2
