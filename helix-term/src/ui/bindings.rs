//! What a shortcut is, where it works, and how it is written down.
//!
//! The screen in `shortcuts.rs` reads the keymap the editor is running and writes what you
//! change back into your `config.toml`; everything about that shape lives here.

use std::collections::HashMap;
use std::path::Path;

use crate::keymap::{KeyTrie, KeyTrieNode, MappableCommand};
use anyhow::Context as _;
use helix_view::{
    document::Mode,
    input::{KeyCode, KeyEvent, KeyModifiers},
};

/// Every mode a shortcut can be given to, in the order the screen shows them.
pub const MODES: [Mode; 3] = [Mode::Normal, Mode::Select, Mode::Insert];

/// Where a shortcut works, which is also the table `config.toml` writes it under.
///
/// There are two worlds, not three modes: typing, and the modal editing that normal and
/// select are two halves of. A shortcut given here belongs to neither — it works
/// wherever you are, which is what a key means in sid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Where {
    /// Wherever you are, whatever the mode: `[keys.all]`.
    Anywhere,
    /// Only while typing: `[keys.insert]`.
    Insert,
    /// Only while editing modally. Normal and select are the same world to whoever uses
    /// it; the two flags remember which of the two tables the shortcut actually lives
    /// in, so changing it writes back exactly where it was.
    Modal { normal: bool, select: bool },
}

impl Where {
    /// The modes it reaches, which are the ones a clash has to be looked for in.
    pub fn modes(self) -> Vec<Mode> {
        match self {
            Where::Anywhere => MODES.to_vec(),
            Where::Insert => vec![Mode::Insert],
            Where::Modal { normal, select } => MODES
                .iter()
                .copied()
                .filter(|mode| match mode {
                    Mode::Normal => normal,
                    Mode::Select => select,
                    Mode::Insert => false,
                })
                .collect(),
        }
    }

    /// The tables in `config.toml` it is written under.
    pub fn tables(self) -> Vec<&'static str> {
        match self {
            Where::Anywhere => vec!["all"],
            Where::Insert => vec!["insert"],
            Where::Modal { normal, select } => {
                let mut tables = Vec::new();
                if normal {
                    tables.push("normal");
                }
                if select {
                    tables.push("select");
                }
                tables
            }
        }
    }

    /// How it reads on screen.
    pub fn label(self) -> &'static str {
        match self {
            Where::Anywhere => "Anywhere",
            Where::Insert => "Insert",
            Where::Modal { .. } => "Modal",
        }
    }
}

/// What a shortcut runs, in the very form `config.toml` writes it.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Runs {
    /// One command: `duplicate_line`, `:write`, or a macro's `@…`.
    One(String),
    /// Several, one after the other.
    Many(Vec<String>),
}

impl Runs {
    /// The command as config.toml spells it: typables keep their colon and their arguments.
    pub fn of(command: &MappableCommand) -> Self {
        Runs::One(written(command))
    }

    /// What it runs, for the search line.
    pub fn text(&self) -> String {
        match self {
            Runs::One(one) => one.clone(),
            Runs::Many(many) => many.join(" "),
        }
    }

    fn value(&self) -> toml_edit::Value {
        match self {
            Runs::One(one) => one.as_str().into(),
            Runs::Many(many) => many
                .iter()
                .map(|one| one.as_str())
                .collect::<toml_edit::Array>()
                .into(),
        }
    }

    /// The same thing as the keymap holds it, so a change shows on screen at once.
    pub fn trie(&self) -> anyhow::Result<KeyTrie> {
        match self {
            Runs::One(one) => Ok(KeyTrie::MappableCommand(one.parse::<MappableCommand>()?)),
            Runs::Many(many) => {
                let commands = many
                    .iter()
                    .map(|one| one.parse::<MappableCommand>())
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(KeyTrie::Sequence(commands))
            }
        }
    }
}

/// How `config.toml` spells a command.
pub fn written(command: &MappableCommand) -> String {
    match command {
        MappableCommand::Typable { name, args, .. } if args.is_empty() => format!(":{name}"),
        MappableCommand::Typable { name, args, .. } => format!(":{name} {args}"),
        other => other.name().to_string(),
    }
}

/// What a shortcut does, in words, as the screen says it.
pub fn describes(trie: &KeyTrie) -> String {
    match trie {
        KeyTrie::MappableCommand(command) => command.doc().to_string(),
        KeyTrie::Sequence(commands) => commands
            .iter()
            .map(|command| command.doc())
            .collect::<Vec<_>>()
            .join("; "),
        KeyTrie::Node(node) => node.infobox().title.into_owned(),
    }
}

/// Walks a keymap gathering every shortcut in it, as keys and what they run.
pub fn walk(
    trie: &KeyTrie,
    path: &mut Vec<KeyEvent>,
    enhanced: bool,
    found: &mut Vec<(Vec<KeyEvent>, Runs, String)>,
) {
    match trie {
        KeyTrie::Node(node) => {
            for (key, child) in node.iter() {
                // A key this terminal never sends is not a shortcut here.
                if !crate::keymap::key_reaches(key, enhanced) {
                    continue;
                }
                path.push(*key);
                walk(child, path, enhanced, found);
                path.pop();
            }
        }
        KeyTrie::MappableCommand(command) if command.name() == "no_op" => {}
        KeyTrie::MappableCommand(command) => {
            found.push((path.clone(), Runs::of(command), command.doc().to_string()))
        }
        KeyTrie::Sequence(commands) => found.push((
            path.clone(),
            Runs::Many(commands.iter().map(written).collect()),
            commands
                .iter()
                .map(|command| command.doc())
                .collect::<Vec<_>>()
                .join("; "),
        )),
    }
}
/// The shortcuts of a keymap, by what they run.
pub type ByAction = HashMap<Runs, Vec<Vec<KeyEvent>>>;

/// Every shortcut of a keymap by what it runs, which is how a command given arguments is
/// told from the same command given others: by name alone every `:toggle-option` is the
/// same command, and they would all show the first one's keys.
pub fn by_action(map: &KeyTrie, enhanced: bool) -> ByAction {
    let mut found = Vec::new();
    walk(map, &mut Vec::new(), enhanced, &mut found);
    let mut by_action = ByAction::new();
    for (keys, runs, _) in found {
        by_action.entry(runs).or_default().push(keys);
    }
    by_action
}

/// What stands in the way of giving a key to an action.
#[derive(Clone, Debug, PartialEq)]
pub enum Clash {
    /// The keys are already another action's, which would lose them.
    Taken { what: String },
    /// The keys begin other shortcuts, which taking them would end.
    Begins { count: usize, first: String },
    /// An earlier key of the sequence already runs something, so the rest is never reached.
    Buried { keys: String, what: String },
    /// The key is text: giving it away would stop it being typed.
    Text,
    /// This terminal never sends the key, so the shortcut would never fire here.
    Unreachable,
}

impl Clash {
    /// The one line the capture box shows about it.
    pub fn says(&self, keys: &str) -> String {
        match self {
            Clash::Taken { what } => format!("{keys} is now «{what}»."),
            Clash::Begins { count, first } => {
                format!("{keys} begins {count} shortcuts, «{first}» among them.")
            }
            Clash::Buried {
                keys: earlier,
                what,
            } => {
                format!("{earlier} already runs «{what}», so {keys} would never be reached.")
            }
            Clash::Text => format!("{keys} is text, and text always types."),
            Clash::Unreachable => format!("This terminal never sends {keys}."),
        }
    }

    /// Whether it stops the shortcut from being given at all.
    pub fn refuses(&self) -> bool {
        matches!(self, Clash::Text)
    }
}

/// Whether the first key is one that types: giving it away would take a letter from the text.
fn types(place: Where, keys: &[KeyEvent]) -> bool {
    if !place.modes().contains(&Mode::Insert) {
        return false;
    }
    let Some(first) = keys.first() else {
        return false;
    };
    let held = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
    matches!(first.code, KeyCode::Char(_)) && !first.modifiers.intersects(held)
}

/// How many shortcuts hang under a node.
fn leaves(trie: &KeyTrie) -> usize {
    match trie {
        KeyTrie::Node(node) => node.values().map(leaves).sum(),
        _ => 1,
    }
}

/// The first shortcut under a node, in words.
fn first_leaf(trie: &KeyTrie) -> String {
    match trie {
        KeyTrie::Node(node) => node.values().next().map(first_leaf).unwrap_or_default(),
        other => describes(other),
    }
}

/// What stands in the way of giving `keys` to an action, if anything does.
pub fn clash(
    maps: &HashMap<Mode, KeyTrie>,
    place: Where,
    keys: &[KeyEvent],
    enhanced: bool,
) -> Option<Clash> {
    if keys.is_empty() {
        return None;
    }
    if types(place, keys) {
        return Some(Clash::Text);
    }
    if !keys
        .iter()
        .all(|key| crate::keymap::key_reaches(key, enhanced))
    {
        return Some(Clash::Unreachable);
    }

    let label = |keys: &[KeyEvent]| {
        keys.iter()
            .map(|key| super::shortcuts::key_label(*key))
            .collect::<Vec<_>>()
            .join(" → ")
    };

    let mut begins = None;
    for mode in place.modes() {
        let Some(map) = maps.get(&mode) else { continue };

        // A key on the way that already runs something buries everything after it.
        for end in 1..keys.len() {
            match map.search(&keys[..end]) {
                Some(trie @ (KeyTrie::MappableCommand(_) | KeyTrie::Sequence(_))) => {
                    return Some(Clash::Buried {
                        keys: label(&keys[..end]),
                        what: describes(trie),
                    })
                }
                _ => continue,
            }
        }

        match map.search(keys) {
            Some(trie @ (KeyTrie::MappableCommand(_) | KeyTrie::Sequence(_))) => {
                // A key left free on purpose is free, not taken.
                if !matches!(trie, KeyTrie::MappableCommand(command) if command.name() == "no_op") {
                    return Some(Clash::Taken {
                        what: describes(trie),
                    });
                }
            }
            Some(trie @ KeyTrie::Node(_)) if begins.is_none() => {
                begins = Some(Clash::Begins {
                    count: leaves(trie),
                    first: first_leaf(trie),
                });
            }
            _ => {}
        }
    }

    begins
}

/// Puts a shortcut into a keymap held in memory, so the screen shows the change at once.
pub fn set(maps: &mut HashMap<Mode, KeyTrie>, place: Where, keys: &[KeyEvent], trie: &KeyTrie) {
    for mode in place.modes() {
        let Some(map) = maps.get_mut(&mode) else {
            continue;
        };
        let Some((last, path)) = keys.split_last() else {
            continue;
        };
        let mut node = match map {
            KeyTrie::Node(node) => node,
            _ => continue,
        };
        for key in path {
            let child = node
                .entry(*key)
                .or_insert_with(|| KeyTrie::Node(KeyTrieNode::default()));
            // A key that ran a command becomes the start of a sequence instead.
            if child.node().is_none() {
                *child = KeyTrie::Node(KeyTrieNode::default());
            }
            node = child.node_mut().expect("it was just made a node");
        }
        node.insert(*last, trie.clone());
    }
}

/// Takes a shortcut out of a keymap held in memory.
pub fn unset(maps: &mut HashMap<Mode, KeyTrie>, place: Where, keys: &[KeyEvent]) {
    for mode in place.modes() {
        let Some(KeyTrie::Node(node)) = maps.get_mut(&mode) else {
            continue;
        };
        drop_at(node, keys);
    }
}

/// Drops the key at the end of the path, if the path is there at all.
fn drop_at(node: &mut KeyTrieNode, keys: &[KeyEvent]) {
    match keys.split_first() {
        Some((last, [])) => {
            node.shift_remove(last);
        }
        Some((key, rest)) => {
            if let Some(child) = node.get_mut(key).and_then(KeyTrie::node_mut) {
                drop_at(child, rest);
            }
        }
        None => {}
    }
}

/// Writes one shortcut into the user's `config.toml`, leaving every other line of it — its
/// comments and its order included — exactly as it was.
pub fn write(path: &Path, place: Where, keys: &[KeyEvent], runs: &Runs) -> anyhow::Result<()> {
    edit(path, place, keys, Some(runs.value()))
}

/// Takes a shortcut away. One of sid's own is written as `no_op`, which is how a key is
/// left doing nothing; one that is only yours is simply dropped.
pub fn erase(path: &Path, place: Where, keys: &[KeyEvent], is_sids: bool) -> anyhow::Result<()> {
    let value = is_sids.then(|| toml_edit::Value::from("no_op"));
    edit(path, place, keys, value)
}

/// Gives the keys back to sid: your line for them is dropped, whatever it said.
pub fn restore(path: &Path, place: Where, keys: &[KeyEvent]) -> anyhow::Result<()> {
    edit(path, place, keys, None)
}

/// Puts a value under every `[keys.<table>]` the place stands for, or takes the entry
/// away when there is none. The file is read and written once, whatever it touches.
fn edit(
    path: &Path,
    place: Where,
    keys: &[KeyEvent],
    value: Option<toml_edit::Value>,
) -> anyhow::Result<()> {
    let (last, chord) = keys
        .split_last()
        .context("a shortcut has at least one key")?;

    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{} is not valid TOML", path.display()))?;

    for table in place.tables() {
        // Every shortcut lives under [keys.<table>], and a chord is a table per key.
        let mut names = vec!["keys".to_string(), table.to_string()];
        names.extend(chord.iter().map(ToString::to_string));
        put(&mut document, path, &names, &last.to_string(), &value)?;
    }

    crate::ui::settings::write_atomically(path, document.to_string())
}

/// One entry written, or dropped when there is no value for it.
fn put(
    document: &mut toml_edit::DocumentMut,
    path: &Path,
    names: &[String],
    last: &str,
    value: &Option<toml_edit::Value>,
) -> anyhow::Result<()> {
    let Some(value) = value else {
        prune(document.as_table_mut(), names, last);
        return Ok(());
    };

    let mut table = document
        .as_table_mut()
        .entry("keys")
        .or_insert_with(implicit);
    for name in names.iter().skip(1) {
        let parent = table
            .as_table_like_mut()
            .with_context(|| format!("'{name}' is not a table in {}", path.display()))?;
        // A key of yours that ran a command on its own gives way to the chord that starts
        // with it: what it ran is exactly what the new shortcut takes the place of. It is
        // dropped and written again, so the line reads as if it had always been a table.
        if parent
            .get(name)
            .is_some_and(|item| item.as_table_like().is_none())
        {
            parent.remove(name);
        }
        table = parent.entry(name).or_insert_with(implicit);
    }
    table
        .as_table_like_mut()
        .with_context(|| format!("'{last}' has no table in {}", path.display()))?
        .insert(last, toml_edit::value(value.clone()));

    Ok(())
}

/// Gives every shortcut back to sid: your whole `[keys]` is dropped, and nothing else in
/// the file is touched.
pub fn restore_all(path: &Path) -> anyhow::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{} is not valid TOML", path.display()))?;
    document.as_table_mut().remove("keys");

    crate::ui::settings::write_atomically(path, document.to_string())
}

/// Drops an entry and every table left empty above it: taking your last shortcut away
/// leaves no `[keys.all]` behind to wonder about. A table written on one line, as
/// `space = { h = "…" }`, is a table like any other here.
fn prune(table: &mut dyn toml_edit::TableLike, names: &[String], last: &str) {
    let Some((name, rest)) = names.split_first() else {
        table.remove(last);
        return;
    };
    let Some(child) = table
        .get_mut(name)
        .and_then(|item| item.as_table_like_mut())
    else {
        return;
    };
    prune(child, rest, last);
    if child.is_empty() {
        table.remove(name);
    }
}

/// A table nobody asked for by name: it is written only if it ends up holding something.
fn implicit() -> toml_edit::Item {
    let mut table = toml_edit::Table::new();
    table.set_implicit(true);

    toml_edit::Item::Table(table)
}

/// Every command the editor has, each as `config.toml` would name it, with what it does.
pub fn catalogue() -> Vec<(Runs, String)> {
    let statics = MappableCommand::STATIC_COMMAND_LIST
        .iter()
        .map(|command| (Runs::of(command), command.doc().to_string()));
    let typables = crate::commands::typed::TYPABLE_COMMAND_LIST
        .iter()
        .map(|command| {
            (
                Runs::One(format!(":{}", command.name)),
                command.doc.to_string(),
            )
        });
    // Flipping a setting is something the editor does, and so something a key can reach:
    // it is offered here under the words the settings screen uses for it.
    let settings = super::settings::as_commands()
        .into_iter()
        .map(|(key, doc)| (Runs::One(format!(":toggle-option {key}")), doc));

    statics.chain(typables).chain(settings).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<KeyEvent> {
        text.split(' ').map(|key| key.parse().unwrap()).collect()
    }

    fn written_name(command: &MappableCommand) -> String {
        written(command)
    }

    fn maps(toml: &str) -> HashMap<Mode, KeyTrie> {
        let trie: KeyTrie = toml::from_str(toml).unwrap();
        MODES.iter().map(|mode| (*mode, trie.clone())).collect()
    }

    #[test]
    fn a_key_another_action_has_is_taken() {
        let maps = maps(r#"C-s = ":write""#);
        let taken = clash(&maps, Where::Anywhere, &keys("C-s"), true);
        assert!(matches!(taken, Some(Clash::Taken { .. })));
        assert!(clash(&maps, Where::Anywhere, &keys("C-j"), true).is_none());
    }

    #[test]
    fn a_key_that_begins_others_says_how_many_it_would_end() {
        let maps = maps("[space]\nh = \"file_history\"\nw = \"goto_word\"\n");
        match clash(
            &maps,
            Where::Modal {
                normal: true,
                select: false,
            },
            &keys("space"),
            true,
        ) {
            Some(Clash::Begins { count, .. }) => assert_eq!(count, 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_key_under_a_command_is_never_reached() {
        let maps = maps(r#"C-s = ":write""#);
        assert!(matches!(
            clash(&maps, Where::Anywhere, &keys("C-s h"), true),
            Some(Clash::Buried { .. })
        ));
    }

    #[test]
    fn text_keeps_typing_and_a_key_the_terminal_drops_is_said_so() {
        let maps = maps("");
        assert_eq!(
            clash(&maps, Where::Anywhere, &keys("a"), true),
            Some(Clash::Text)
        );
        assert_eq!(
            clash(&maps, Where::Insert, &keys("A"), true),
            Some(Clash::Text)
        );
        // Out of insert, a letter is a shortcut like any other.
        assert!(clash(
            &maps,
            Where::Modal {
                normal: true,
                select: false
            },
            &keys("a"),
            true
        )
        .is_none());
        assert_eq!(
            clash(&maps, Where::Anywhere, &keys("Cmd-j"), false),
            Some(Clash::Unreachable)
        );
    }

    #[test]
    fn every_shortcut_goes_back_at_once_and_nothing_else_does() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "theme = \"github_dark\"\n\n[editor]\nmouse = true\n").unwrap();
        write(
            &path,
            Where::Anywhere,
            &keys("A-C-j"),
            &Runs::One("duplicate_line".into()),
        )
        .unwrap();
        write(
            &path,
            Where::Insert,
            &keys("F7"),
            &Runs::One("select_all".into()),
        )
        .unwrap();

        restore_all(&path).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("keys"), "{written}");
        assert!(written.contains("theme = \"github_dark\""));
        assert!(written.contains("mouse = true"));
    }

    #[test]
    fn a_shortcut_of_the_modal_world_is_written_to_both_its_tables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let both = Where::Modal {
            normal: true,
            select: true,
        };
        write(&path, both, &keys("F7"), &Runs::One("select_all".into())).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("[keys.normal]\nF7 = \"select_all\""),
            "{written}"
        );
        assert!(
            written.contains("[keys.select]\nF7 = \"select_all\""),
            "{written}"
        );

        erase(&path, both, &keys("F7"), false).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("F7"), "{written}");
    }

    #[test]
    fn a_table_written_on_one_line_is_pruned_like_any_other() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            "theme = \"github_dark\"\n\n[keys.normal]\nspace = { h = \"file_history\", w = \"goto_word\" }\n",
        )
        .unwrap();
        let normal = Where::Modal {
            normal: true,
            select: false,
        };

        erase(&path, normal, &keys("space h"), false).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("file_history"), "{written}");
        assert!(written.contains("goto_word"), "{written}");

        // The last one out takes the table with it, and [keys] as well.
        restore(&path, normal, &keys("space w")).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("keys"), "{written}");
        assert!(written.contains("theme = \"github_dark\""));
    }

    #[test]
    fn a_key_left_doing_nothing_is_free() {
        let maps = maps(r#"C-u = "no_op""#);
        assert!(clash(&maps, Where::Anywhere, &keys("C-u"), true).is_none());
    }

    #[test]
    fn a_shortcut_is_written_where_it_belongs_and_taken_away_whole() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "# mine, and it stays\ntheme = \"github_dark\"\n").unwrap();

        write(
            &path,
            Where::Anywhere,
            &keys("C-A-j"),
            &Runs::One("duplicate_line".into()),
        )
        .unwrap();
        write(
            &path,
            Where::Insert,
            &keys("space h"),
            &Runs::Many(vec![":write".into(), "duplicate_line".into()]),
        )
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# mine, and it stays"));
        assert!(written.contains("[keys.all]\nA-C-j = \"duplicate_line\""));
        assert!(written.contains("[keys.insert.space]\nh = [\":write\", \"duplicate_line\"]"));

        // What it wrote is what the editor reads back, in the modes it was meant for.
        let text = crate::config::over_defaults(&written).unwrap();
        let config = crate::config::Config::load(Ok(&text), Err(Default::default())).unwrap();
        let at = |mode: Mode, path: &str| match config.keys[&mode].search(&keys(path)) {
            Some(KeyTrie::MappableCommand(command)) => written_name(command),
            Some(KeyTrie::Sequence(commands)) => commands
                .iter()
                .map(written_name)
                .collect::<Vec<_>>()
                .join(" "),
            other => panic!("{path} in {mode:?} is {other:?}"),
        };
        for mode in MODES {
            assert_eq!(at(mode, "A-C-j"), "duplicate_line");
        }
        assert_eq!(at(Mode::Insert, "space h"), ":write duplicate_line");

        erase(&path, Where::Anywhere, &keys("C-s"), true).unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("C-s = \"no_op\""));

        restore(&path, Where::Insert, &keys("space h")).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("keys.insert"), "{written}");
        assert!(written.contains("A-C-j = \"duplicate_line\""));

        // A chord takes the place of a key of yours that ran a command on its own.
        write(
            &path,
            Where::Anywhere,
            &keys("A-C-j t"),
            &Runs::One("select_all".into()),
        )
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("A-C-j = \"duplicate_line\""), "{written}");
        assert!(
            written.contains("[keys.all.A-C-j]\nt = \"select_all\""),
            "{written}"
        );
        let text = crate::config::over_defaults(&written).unwrap();
        crate::config::Config::load(Ok(&text), Err(Default::default())).unwrap();
    }
}
