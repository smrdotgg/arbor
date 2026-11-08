//! What Arbor exposes to assistive technology (screen readers, Voice Control, automation):
//! the AccessKit tree Rune builds from the view, read from a headless runtime running the real
//! view, keys and handlers. Each test prints an outline of the part it checks (role, key,
//! name, states, ...; no bounds), which is the snapshot it compares.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use rune::{AccessibilityLink, App, AppSpec, Env, Key, KeyEvent, Mods, Runtime};
use serde_json::Value;

use crate::app::State;
use crate::commands::Cmd;

thread_local! {
    /// The folder the next runtime opens (`AppSpec::init` takes no arguments).
    static DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn init() -> State {
    let d = DIR.with_borrow(|d| d.clone()).expect("a folder");
    let mut s = crate::app::tests::state_in(&d);
    crate::app::tests::settle_all(&mut s);
    s.places = vec![("Home".into(), crate::fs::home()), ("Test".into(), d)];
    s
}

fn folder(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-a11y-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("Photos")).unwrap();
    std::fs::write(d.join("notes.txt"), "hello").unwrap();
    std::fs::write(d.join("todo.md"), "- [ ] a").unwrap();
    d
}

/// The real app (view, keys, poll) without a window, on `d`.
fn runtime(d: &Path) -> Runtime<State> {
    DIR.with_borrow_mut(|slot| *slot = Some(d.to_path_buf()));
    let spec = AppSpec::<State> {
        poll: Some(|s, _, tx| s.poll(tx)),
        on_key: Some(|s, ev, tx| s.on_key(ev, tx)),
        ..AppSpec::new("Arbor", (980.0, 620.0), init, crate::view::view)
    };
    let mut rt = App::new(spec).headless_runtime((980.0, 620.0), Env::new(false, 1.0));
    rt.frame(0.0);
    rt.frame(0.0);
    rt
}

fn key(rt: &mut Runtime<State>, key: Key, shift: bool) {
    rt.key(KeyEvent { key, mods: Mods { shift, ..Mods::default() } });
    rt.frame(0.0);
}

fn run(rt: &mut Runtime<State>, cmd: Cmd) {
    rt.update(move |s, tx| s.run(cmd, tx));
    rt.frame(0.0);
    rt.frame(0.0);
}

fn find<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    if v["key"] == key {
        return Some(v);
    }
    v["children"].as_array()?.iter().find_map(|c| find(c, key))
}

fn find_by<'a>(v: &'a Value, f: &dyn Fn(&Value) -> bool) -> Option<&'a Value> {
    if f(v) {
        return Some(v);
    }
    v["children"].as_array()?.iter().find_map(|c| find_by(c, f))
}

fn all<'a>(v: &'a Value, role: &str, out: &mut Vec<&'a Value>) {
    if v["role"] == role {
        out.push(v);
    }
    for c in v["children"].as_array().into_iter().flatten() {
        all(c, role, out);
    }
}

fn states(v: &Value) -> Vec<&str> {
    v["states"].as_array().map_or(Vec::new(), |s| s.iter().filter_map(Value::as_str).collect())
}

/// One line per node: role, key, name and what a reader is told besides (states, value,
/// description, shortcut, place in its set, active descendant, sort), indented by depth.
/// Text runs, bounds and actions are left out.
fn outline(v: &Value) -> String {
    fn walk(v: &Value, depth: usize, out: &mut String) {
        let mut line = format!("{}{}", "  ".repeat(depth), v["role"].as_str().unwrap_or("?"));
        if let Some(k) = v["key"].as_str() {
            line += &format!(" #{k}");
        }
        if let Some(n) = v["name"].as_str() {
            line += &format!(" {n:?}");
        }
        for (field, tag) in
            [("value", "value"), ("description", "desc"), ("shortcut", "keys"), ("activeDescendant", "active"), ("sort", "sort")]
        {
            if let Some(x) = v[field].as_str() {
                line += &format!(" {tag}={x:?}");
            }
        }
        if let Some(p) = v["position"].as_u64() {
            line += &format!(" pos={p}");
        }
        if let Some(n) = v["setSize"].as_u64() {
            line += &format!(" of={n}");
        }
        let st: Vec<&str> = states(v).into_iter().filter(|s| *s != "clips").collect();
        if !st.is_empty() {
            line += &format!(" [{}]", st.join(" "));
        }
        out.push_str(&line);
        out.push('\n');
        for c in v["children"].as_array().into_iter().flatten() {
            walk(c, depth + 1, out);
        }
    }
    let mut out = String::new();
    walk(v, 0, &mut out);
    out
}

/// The main view: sidebar places, the path bar's buttons and breadcrumbs, the column headers,
/// and the file list as a list box of options named from their cells, the selected one
/// selected, each with its place in the folder; the status bar's live region.
#[test]
fn the_main_view_names_every_part() {
    let d = folder("main");
    let mut rt = runtime(&d);
    let tree = rt.accessibility_json();
    let o = outline(&tree);
    println!("{o}");
    let name = d.file_name().unwrap().to_string_lossy().into_owned();
    // The sidebar: a list box of places, the one on screen selected, shortcut as a property.
    let places = find(&tree, "sidebar").expect("sidebar");
    assert_eq!((places["role"].as_str(), places["name"].as_str()), (Some("list-box"), Some("Places")));
    let test = find(&tree, "place-1").unwrap();
    assert_eq!(test["name"], "Test", "{o}");
    assert!(states(test).contains(&"selected"), "the folder on screen: {test}");
    assert!(test["shortcut"].as_str().is_some_and(|k| k.ends_with('2')), "{test}");
    // The path bar: named buttons (Back can't go anywhere yet), breadcrumbs as links.
    let back = find(&tree, "nav-back").unwrap();
    assert_eq!((back["role"].as_str(), back["name"].as_str()), (Some("button"), Some("Go Back")));
    assert!(states(back).contains(&"disabled"));
    assert_eq!(find(&tree, "nav-up").unwrap()["name"], "Go to Parent Folder");
    let crumbs = find(&tree, "crumbs").unwrap();
    assert_eq!((crumbs["role"].as_str(), crumbs["name"].as_str()), (Some("navigation"), Some("Path")));
    let mut links = Vec::new();
    all(crumbs, "link", &mut links);
    assert_eq!(links.last().and_then(|l| l["name"].as_str()), Some(name.as_str()), "{o}");
    assert!(!o.contains("\"›\""), "separators are left out: {o}");
    // Column headers, the sorted one saying how.
    let head = find(&tree, "sort-Name").unwrap();
    assert_eq!(
        (head["role"].as_str(), head["name"].as_str(), head["sort"].as_str()),
        (Some("column-header"), Some("Name"), Some("ascending"))
    );
    assert!(find(&tree, "sort-Size").unwrap()["sort"].is_null());
    // The list: a list box named for its folder, whose active descendant is the cursor's row.
    let list = find(&tree, "list-0").expect("the file list");
    assert_eq!(list["role"], "list-box");
    assert_eq!(list["name"], format!("Files in {name}"));
    assert_eq!(list["activeDescendant"], "f:Photos", "{list}");
    let mut rows = Vec::new();
    all(list, "list-box-option", &mut rows);
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(rows.len(), 3, "{o}");
    assert!(names[0].starts_with("Photos Folder") && !names[0].contains('—') && !names[0].contains('▸'), "{names:?}");
    assert!(names[1].starts_with("notes.txt "), "name first, then its cells: {names:?}");
    assert!(states(rows[0]).contains(&"selected") && !states(rows[1]).contains(&"selected"));
    // "2 of 3": the row's place, and the size of the whole list (not just the rows built).
    assert_eq!(rows[1]["position"].as_u64(), Some(2));
    assert_eq!(find(list, "rows-0").unwrap()["setSize"].as_u64(), Some(3), "{o}");
    // ↓ moves the selection, and the list's active descendant with it.
    key(&mut rt, Key::Down, false);
    let tree = rt.accessibility_json();
    assert_eq!(find(&tree, "list-0").unwrap()["activeDescendant"], "f:notes.txt");
}

/// A narrow pane hides the Kind column: a folder's row still says it's a folder.
#[test]
fn a_folder_says_so_without_the_kind_column() {
    let d = folder("split");
    let mut rt = runtime(&d);
    run(&mut rt, Cmd::Split);
    let tree = rt.accessibility_json();
    let list = find(&tree, "list-1").expect("the second pane's list");
    let mut rows = Vec::new();
    all(list, "list-box-option", &mut rows);
    assert!(rows[0]["name"].as_str().is_some_and(|n| n.starts_with("Folder Photos")), "{}", outline(list));
    // Both lists point a reader at the active pane's cursor row, whichever holds focus.
    let left = find(&tree, "list-0").unwrap();
    assert_eq!(left["activeDescendant"], list["activeDescendant"]);
}

/// Operation results are announced: the status note is a polite live region, an error an
/// assertive one (an alert). With a client connected, the change reaches it as an update.
#[test]
fn results_and_errors_are_announced() {
    let d = folder("announce");
    let mut rt = runtime(&d);
    let mut link = AccessibilityLink::new();
    link.activate();
    assert!(link.service(&mut rt, "Arbor").is_some_and(|u| u.tree.is_some()), "the whole tree on activation");
    // Trash notes.txt: "Moved 1 item to the Trash".
    rt.update(|s, _| {
        let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "notes.txt").unwrap();
        s.cur_mut().select(i);
    });
    run(&mut rt, Cmd::Trash);
    rt.update(|s, _| crate::app::tests::finish_job(s));
    rt.frame(0.0);
    let tree = rt.accessibility_json();
    let note = find(&tree, "note").expect("a note");
    assert_eq!(note["role"], "status");
    assert!(note["name"].as_str().is_some_and(|n| n.starts_with("Moved 1 item to the")), "{note}");
    assert!(states(note).contains(&"live-polite"));
    let update = link.service(&mut rt, "Arbor").expect("the change is sent");
    assert!(
        update.nodes.iter().any(|(_, n)| n.author_id() == Some("note") && n.live() == Some(rune::accesskit::Live::Polite)),
        "the note is in the update"
    );
    // A refused undo: the note becomes an alert.
    std::fs::write(d.join("notes.txt"), "back in the way").unwrap();
    run(&mut rt, Cmd::Undo);
    rt.update(|s, _| crate::app::tests::finish_job(s));
    rt.frame(0.0);
    let tree = rt.accessibility_json();
    let note = find(&tree, "note").expect("a note");
    assert_eq!(note["role"], "alert", "{note}");
    assert!(states(note).contains(&"live-assertive"));
    assert!(note["name"].as_str().is_some_and(|n| n.starts_with("Can't undo")), "{note}");
    // Undone fine: polite again.
    std::fs::remove_file(d.join("notes.txt")).unwrap();
    run(&mut rt, Cmd::Undo);
    rt.update(|s, _| crate::app::tests::finish_job(s));
    rt.frame(0.0);
    let note = find(&rt.accessibility_json(), "note").cloned().expect("a note");
    assert_eq!((note["role"].as_str(), note["name"].as_str()), (Some("status"), Some("Restored 1 item")));
}

/// Until an assistive client asks, nothing is built: Rune builds the tree only on activation,
/// and Arbor never forces it. A link nobody activated gets nothing, frame after frame.
#[test]
fn nothing_is_built_until_a_client_asks() {
    let d = folder("lazy");
    let mut rt = runtime(&d);
    let mut link = AccessibilityLink::new();
    for _ in 0..3 {
        key(&mut rt, Key::Down, false);
        assert!(link.service(&mut rt, "Arbor").is_none());
    }
    link.activate();
    assert!(link.service(&mut rt, "Arbor").is_some());
    // Idle afterwards: nothing to send.
    rt.frame(0.0);
    assert!(link.service(&mut rt, "Arbor").is_none(), "an idle frame sends nothing");
}

fn focused_key(tree: &Value) -> Option<String> {
    find_by(tree, &|v| states(v).contains(&"focused")).and_then(|v| v["key"].as_str()).map(str::to_owned)
}

/// Tab puts focus on the list; an overlay takes it (and keeps it: the background leaves the
/// tree) and gives it back when it closes.
#[test]
fn overlays_trap_focus_and_give_it_back() {
    let d = folder("focus");
    let mut rt = runtime(&d);
    key(&mut rt, Key::Tab, false);
    assert_eq!(focused_key(&rt.accessibility_json()).as_deref(), Some("list-0"), "Tab reaches the list");
    for (cmd, field) in [(Cmd::Palette, "palette-field"), (Cmd::Jump, "jump-field"), (Cmd::SelectPattern, "pick-field")] {
        run(&mut rt, cmd);
        let tree = rt.accessibility_json();
        assert_eq!(focused_key(&tree).as_deref(), Some(field), "{cmd:?}: {}", outline(&tree));
        assert!(find(&tree, "list-0").is_none(), "{cmd:?}: the background leaves the tree");
        key(&mut rt, Key::Tab, false);
        let tree = rt.accessibility_json();
        assert!(focused_key(&tree).is_some_and(|k| k.starts_with(&field[..4])), "{cmd:?}: Tab stays inside: {:?}", focused_key(&tree));
        key(&mut rt, Key::Escape, false);
        assert_eq!(focused_key(&rt.accessibility_json()).as_deref(), Some("list-0"), "{cmd:?}: focus comes back");
    }
}

/// The command palette: a modal dialog with a named field whose active descendant is the
/// highlighted command; options carry their shortcut, unavailable ones are disabled.
#[test]
fn the_palette_tree() {
    let d = folder("palette");
    let mut rt = runtime(&d);
    run(&mut rt, Cmd::Palette);
    let tree = rt.accessibility_json();
    let p = find(&tree, "palette").expect("palette");
    println!("{}", outline(p));
    assert_eq!((p["role"].as_str(), p["name"].as_str()), (Some("dialog"), Some("Command Palette")));
    assert!(states(p).contains(&"modal"));
    let field = find(p, "palette-field").unwrap();
    assert_eq!((field["role"].as_str(), field["name"].as_str()), (Some("text-input"), Some("Command")));
    let first = field["activeDescendant"].as_str().expect("the highlighted command").to_owned();
    let rows = find(p, "palette-rows").unwrap();
    assert_eq!(rows["role"], "list-box");
    let mut opts = Vec::new();
    all(rows, "list-box-option", &mut opts);
    assert_eq!(opts[0]["key"], first.as_str());
    assert!(states(opts[0]).contains(&"selected"));
    let open = opts.iter().find(|o| o["name"] == "Open").expect("Open");
    assert!(open["shortcut"].as_str().is_some_and(|k| !k.is_empty()), "{open}");
    let back = opts.iter().find(|o| o["name"] == "Go Back").expect("Go Back");
    assert!(states(back).contains(&"disabled"), "nowhere to go back to: {back}");
    key(&mut rt, Key::Down, false);
    let tree = rt.accessibility_json();
    assert_ne!(find(&tree, "palette-field").unwrap()["activeDescendant"], first.as_str(), "↓ moves it");
}

/// The confirm dialog: an alert dialog named by its title, described by what it says, whose
/// active descendant is the button with the focus (Cancel first; Tab moves it).
#[test]
fn the_confirm_dialog_is_an_alert_dialog() {
    let d = folder("confirm");
    let mut rt = runtime(&d);
    rt.update(|s, _| {
        let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "todo.md").unwrap();
        s.cur_mut().select(i);
    });
    run(&mut rt, Cmd::Delete);
    let tree = rt.accessibility_json();
    let c = find(&tree, "confirm").expect("the dialog");
    println!("{}", outline(c));
    assert_eq!(c["role"], "alert-dialog");
    assert_eq!(c["name"], "Delete “todo.md” Permanently?");
    assert!(c["description"].as_str().is_some_and(|d| d.ends_with("This can't be undone.")), "{c}");
    assert_eq!(c["activeDescendant"], "confirm-cancel");
    assert_eq!(focused_key(&tree).as_deref(), Some("confirm"), "the dialog has the keys");
    assert!(states(c).contains(&"modal"));
    assert_eq!(find(c, "confirm-action").unwrap()["name"], "Delete Permanently");
    key(&mut rt, Key::Tab, false);
    assert_eq!(find(&rt.accessibility_json(), "confirm").unwrap()["activeDescendant"], "confirm-action");
    key(&mut rt, Key::Escape, false);
    assert!(d.join("todo.md").exists(), "Esc cancels");
}

/// Jump, Select by Pattern and Batch Rename: named dialogs with named fields, and their live
/// counts and summaries as status regions.
#[test]
fn jump_pattern_and_batch_rename_trees() {
    let d = folder("boxes");
    let mut rt = runtime(&d);
    run(&mut rt, Cmd::Jump);
    let tree = rt.accessibility_json();
    let j = find(&tree, "jump").expect("jump");
    println!("{}", outline(j));
    assert_eq!((j["role"].as_str(), j["name"].as_str()), (Some("dialog"), Some("Jump to Folder")));
    assert_eq!(find(j, "jump-field").unwrap()["name"], "Folder");
    key(&mut rt, Key::Escape, false);

    run(&mut rt, Cmd::SelectPattern);
    let tree = rt.accessibility_json();
    let p = find(&tree, "pick").expect("pick");
    println!("{}", outline(p));
    assert_eq!(p["name"], "Select by Pattern");
    assert_eq!(find(p, "pick-field").unwrap()["name"], "Pattern");
    let count = find(p, "pick-count").unwrap();
    assert_eq!((count["role"].as_str(), count["name"].as_str()), (Some("status"), Some("3 items here")));
    key(&mut rt, Key::Escape, false);

    rt.update(|s, _| s.cur_mut().select_all());
    run(&mut rt, Cmd::BatchRename);
    let tree = rt.accessibility_json();
    let b = find(&tree, "batch").expect("batch");
    println!("{}", outline(b));
    assert_eq!(b["name"], "Batch Rename");
    for (k, name) in
        [("batch-find", "Find"), ("batch-replace", "Replace with"), ("batch-start", "Number start"), ("batch-step", "Number step")]
    {
        assert_eq!(find(b, k).unwrap()["name"], name);
    }
    let regex = find(b, "batch-regex").unwrap();
    assert_eq!((regex["role"].as_str(), regex["name"].as_str()), (Some("switch"), Some("Regular expression")));
    assert!(states(regex).contains(&"unchecked"));
    let ext = find(b, "batch-ext").unwrap();
    assert_eq!((ext["role"].as_str(), ext["name"].as_str()), (Some("check-box"), Some("Include extension")));
    let cases = find(b, "batch-cases").unwrap();
    assert_eq!(cases["role"], "radio-group");
    let mut radios = Vec::new();
    all(cases, "radio-button", &mut radios);
    assert_eq!(radios.len(), 4);
    assert_eq!(radios.iter().filter(|r| states(r).contains(&"checked")).count(), 1);
    assert_eq!(find(b, "batch-summary").unwrap()["role"], "status");
    assert!(states(find(b, "batch-apply").unwrap()).contains(&"disabled"), "nothing changes yet");
}

/// The in-window context menu (Linux): a menu whose items carry their shortcuts, what can't
/// run disabled, and the highlighted item as its active descendant.
#[test]
fn the_in_window_menu_tree() {
    let d = folder("menu");
    let mut rt = runtime(&d);
    rt.update(|s, tx| {
        let items = s.menu_items(true);
        s.open_menu(items, rune::PointerPress { x: 300.0, y: 200.0 }, tx);
    });
    rt.frame(0.0);
    key(&mut rt, Key::Down, false);
    let tree = rt.accessibility_json();
    let m = find(&tree, "menu").expect("menu");
    println!("{}", outline(m));
    assert_eq!((m["role"].as_str(), m["name"].as_str()), (Some("menu"), Some("Actions")));
    assert_eq!(m["activeDescendant"], "menu-0");
    let mut items = Vec::new();
    all(m, "menu-item", &mut items);
    assert_eq!(items[0]["name"], "Open");
    assert!(items[0]["shortcut"].as_str().is_some_and(|k| !k.is_empty()));
    let undo = items.iter().find(|i| i["name"] == "Undo").unwrap();
    assert!(states(undo).contains(&"disabled"));
}

/// The preview panel: a complementary region named Preview, the item's name as a heading, and
/// a text file's contents as a document Tab reaches, which ↓ scrolls.
#[test]
fn the_preview_panel_tree() {
    let d = folder("preview");
    let long: String = (0..400).map(|i| format!("line {i}\n")).collect();
    std::fs::write(d.join("notes.txt"), long).unwrap();
    let mut rt = runtime(&d);
    rt.update(|s, _| {
        let i = s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].name == "notes.txt").unwrap();
        s.cur_mut().select(i);
    });
    run(&mut rt, Cmd::TogglePreview);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while rt.state.preview.as_ref().is_some_and(|p| p.is_loading()) && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(5));
        rt.frame(0.0);
    }
    rt.frame(0.0);
    let tree = rt.accessibility_json();
    let p = find(&tree, "preview").expect("preview");
    assert_eq!((p["role"].as_str(), p["name"].as_str()), (Some("complementary"), Some("Preview")));
    let heading = find_by(p, &|v| v["role"] == "heading").unwrap();
    assert_eq!(heading["name"], "notes.txt");
    let doc = find_by(p, &|v| v["role"] == "document").expect("the contents");
    let doc_key = doc["key"].as_str().unwrap().to_owned();
    // Tab: the list, then the contents.
    key(&mut rt, Key::Tab, false);
    key(&mut rt, Key::Tab, false);
    assert_eq!(focused_key(&rt.accessibility_json()).as_deref(), Some(doc_key.as_str()));
    key(&mut rt, Key::Down, false);
    rt.frame(0.0);
    let doc = find(&rt.accessibility_json(), &doc_key).cloned().unwrap();
    assert!(doc["scroll"][1].as_f64().is_some_and(|y| y > 0.0), "↓ scrolled it: {doc}");
    assert_eq!(rt.state.cur().selected().map(|e| e.name.clone()).as_deref(), Some("notes.txt"), "and didn't move the list");
}

/// Go-to's completion card: a list box of the candidates, the one in the field selected.
#[test]
fn the_completion_card_tree() {
    let d = folder("complete");
    std::fs::create_dir(d.join("Photographs")).unwrap();
    let mut rt = runtime(&d);
    let typed = format!("{}/Pho", d.display());
    rt.update(move |s, _| s.cur_mut().type_filter(&typed));
    key(&mut rt, Key::Tab, false);
    key(&mut rt, Key::Tab, false);
    let tree = rt.accessibility_json();
    let c = find(&tree, "complete").expect("the card");
    println!("{}", outline(c));
    assert_eq!((c["role"].as_str(), c["name"].as_str()), (Some("list-box"), Some("Completions")));
    let mut opts = Vec::new();
    all(c, "list-box-option", &mut opts);
    assert_eq!(opts.len(), 2);
    assert!(states(opts[0]).contains(&"selected"));
    let filter = find(&tree, "filter").unwrap();
    assert!(filter["name"].as_str().is_some_and(|n| n.starts_with("Go to: ") && n.ends_with("Photographs/")), "{filter}");
}
