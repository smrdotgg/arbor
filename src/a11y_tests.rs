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

/// Get Info's total, a status, reads what it counts: a selected folder's contents ("Empty",
/// not "1 item", for an empty one), and for several items how many are selected apart from
/// what's inside them.
#[test]
fn get_info_total_says_what_it_counts() {
    let d = folder("info");
    std::fs::write(d.join("Photos/a.jpg"), "jpg").unwrap();
    std::fs::create_dir(d.join("Empty")).unwrap();
    let mut rt = runtime(&d);
    let total = |rt: &mut Runtime<State>| {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            rt.frame(0.0);
            let tree = rt.accessibility_json();
            let t = find(&tree, "info-total").expect("the total").clone();
            if !t["name"].as_str().unwrap_or("").ends_with("so far…") || std::time::Instant::now() > until {
                println!("{}", outline(&t));
                assert_eq!(t["role"], "status");
                return t["name"].as_str().unwrap_or("").to_string();
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    };
    let at = |s: &State, n: &str| s.cur().visible.iter().position(|&e| s.cur().entries[e as usize].os == n).unwrap();
    rt.update(move |s, _| {
        let i = at(s, "Empty");
        s.cur_mut().select(i);
    });
    run(&mut rt, Cmd::Info);
    assert!(total(&mut rt).contains("Empty"));
    rt.update(move |s, _| {
        let i = at(s, "Photos");
        s.cur_mut().select(i);
        s.sync_preview();
    });
    assert!(total(&mut rt).contains("1 item · 3 B"));
    rt.update(|s, _| {
        s.cur_mut().select_all();
        s.sync_preview();
    });
    assert!(total(&mut rt).contains("4 selected · 1 item inside · 15 B"));
}

// ---- Keys after a drag (the real app's runtime, as above) ----------------------------------

/// The centre of the list row named `name` (its accessibility bounds).
fn row_centre(rt: &mut Runtime<State>, name: &str) -> (f32, f32) {
    let tree = rt.accessibility_json();
    let row = find_by(&tree, &|v| v["role"] == "list-box-option" && v["name"].as_str().is_some_and(|n| n.starts_with(name)))
        .unwrap_or_else(|| panic!("a row named {name}"));
    let b: Vec<f32> = row["bounds"].as_array().expect("bounds").iter().map(|x| x.as_f64().unwrap() as f32).collect();
    (b[0] + b[2] * 0.5, b[1] + b[3] * 0.5)
}

/// Drags the row `from` onto the row `onto` and lets go there, with nothing taking the drop.
fn drag_row(rt: &mut Runtime<State>, from: &str, onto: &str) {
    let (a, b) = (row_centre(rt, from), row_centre(rt, onto));
    // Selected first, as the QA session did.
    rt.pointer_move(a.0, a.1);
    rt.pointer_down(a.0, a.1);
    rt.pointer_up(a.0, a.1);
    rt.frame(0.0);
    rt.frame(1.0);
    rt.pointer_down(a.0, a.1);
    rt.frame(0.0);
    rt.pointer_move(a.0 + 2.0, (a.1 + b.1) * 0.5);
    rt.frame(0.0);
    rt.pointer_move(b.0, b.1);
    rt.pointer_up(b.0, b.1);
    rt.frame(0.0);
    rt.frame(0.0);
}

/// QA (macOS): after a single-pane drag of a file onto a folder row that nothing took, the
/// next key reaches its command (F2 renames), with no Esc first. Both ways it ends: a drag
/// still held when its system session started, and one whose button came up before the session
/// took over the mouse (the real-Mac log: the release reached the window 22 ms before Rune said
/// the session had started, and the session waited for another until Esc). That one is given
/// its release again, once, and says how to move files.
#[test]
fn after_a_drag_nothing_took_the_next_key_reaches_its_command() {
    let d = folder("drag-keys");
    for released in [false, true] {
        let mut rt = runtime(&d);
        crate::actions::RELEASES_POSTED.with(|n| n.set(0));
        crate::actions::PRETEND_PRESSED.with(|c| c.set(Some(if released { 0 } else { 1 })));
        drag_row(&mut rt, "notes.txt", "Photos");
        crate::actions::PRETEND_PRESSED.with(|c| c.set(None));
        let posted = crate::actions::RELEASES_POSTED.with(std::cell::Cell::get);
        assert_eq!(posted, usize::from(released), "a release given again only when the button was up (released: {released})");
        assert_eq!(rt.state.note.as_deref(), Some(crate::actions::DRAG_NOT_TAKEN), "it says how to move files");
        assert_eq!(rt.state.cur().selected().map(|e| e.name.clone()).as_deref(), Some("notes.txt"));
        key(&mut rt, Key::Function(2), false);
        assert!(rt.state.rename.is_some(), "F2 renames right after the drag (released: {released})");
        assert!(d.join("notes.txt").exists() && !d.join("Photos/notes.txt").exists(), "nothing moved");
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// The decision behind it: a session that starts with the left button up was left behind by
/// its release; with the button (or the left and another) down it wasn't; with no word from
/// the window server (off macOS) nothing is done.
#[test]
fn a_drag_session_is_left_behind_only_with_the_left_button_up() {
    use crate::actions::drag_left_behind;
    assert!(drag_left_behind(Some(0)));
    assert!(drag_left_behind(Some(2)), "only the right button down");
    assert!(!drag_left_behind(Some(1)));
    assert!(!drag_left_behind(Some(3)));
    assert!(!drag_left_behind(None));
}

// ---- Scrolling the text preview sideways -----------------------------------------------------

/// The real app with `name`'s preview open on a file of long lines; the contents' key and the
/// centre of its box.
fn long_line_preview(name: &str) -> (PathBuf, Runtime<State>, String, (f32, f32)) {
    let d = folder(name);
    let long: String = (0..60).map(|i| format!("{i:03} {}\n", "abcdefghij".repeat(30))).collect();
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
    rt.frame(0.0);
    let tree = rt.accessibility_json();
    let doc = find_by(&tree, &|v| v["role"] == "document").expect("the contents");
    let key = doc["key"].as_str().unwrap().to_owned();
    let b: Vec<f32> = doc["bounds"].as_array().expect("bounds").iter().map(|x| x.as_f64().unwrap() as f32).collect();
    (d, rt, key, (b[0] + b[2] * 0.5, b[1] + b[3] * 0.5))
}

/// Where the preview's contents are scrolled to, as assistive technology is told.
fn scrolled(rt: &mut Runtime<State>, key: &str) -> [f64; 2] {
    let doc = find(&rt.accessibility_json(), key).cloned().unwrap();
    [doc["scroll"][0].as_f64().unwrap_or(0.0), doc["scroll"][1].as_f64().unwrap_or(0.0)]
}

/// QA: long lines in the text preview scroll sideways with a horizontal wheel or a trackpad's
/// two-finger swipe (pixel deltas, with phases), not only with ← →; a mouse wheel's vertical
/// delta still scrolls it down.
#[test]
fn the_text_preview_scrolls_sideways_with_the_wheel_and_trackpad() {
    use rune::{GesturePhase, WheelEvent};
    let (d, mut rt, key, (x, y)) = long_line_preview("hscroll");
    rt.pointer_move(x, y);
    rt.frame(0.0);
    assert_eq!(scrolled(&mut rt, &key), [0.0, 0.0]);
    // A mouse's horizontal wheel (or a tilt): one delta, no phases.
    rt.wheel_xy(x, y, -120.0, 0.0);
    rt.frame(0.0);
    let after = scrolled(&mut rt, &key);
    assert!(after[0] >= 100.0 && after[1] == 0.0, "the wheel moved it right: {after:?}");
    // A trackpad swipe back: pixel deltas with phases, mostly sideways.
    rt.frame(1.0);
    for (i, dx) in [8.0, 20.0, 30.0, 12.0].into_iter().enumerate() {
        let phase = if i == 0 { GesturePhase::Began } else { GesturePhase::Moved };
        rt.wheel_event(WheelEvent::new(dx, 1.0).at(x, y).phase(phase).precise(true));
        rt.frame(0.016);
    }
    rt.wheel_event(WheelEvent::new(0.0, 0.0).at(x, y).phase(GesturePhase::Ended).precise(true));
    rt.frame(0.016);
    let back = scrolled(&mut rt, &key);
    assert!(back[0] < after[0] - 60.0 && back[1] == 0.0, "the swipe moved it back left, not down: {after:?} → {back:?}");
    // The plain wheel still scrolls it down.
    rt.frame(1.0);
    rt.wheel_xy(x, y, 0.0, -80.0);
    rt.frame(0.0);
    assert!(scrolled(&mut rt, &key)[1] > 0.0);
    let _ = std::fs::remove_dir_all(&d);
}

/// Shift + a mouse wheel scrolls the text preview sideways even when the delta arrives
/// unturned (vertical, Shift held): how macOS delivers a wheel that a mouse utility, Screen
/// Sharing or an automation tool posts. Two notches in one frame both count; Shift's turn
/// leaves the vertical position alone, and a trackpad's delta (precise) isn't turned.
#[test]
fn shift_and_the_wheel_scroll_the_text_preview_sideways() {
    use rune::{Mods, WheelEvent};
    let (d, mut rt, key, (x, y)) = long_line_preview("shift-wheel");
    rt.pointer_move(x, y);
    rt.frame(0.0);
    let shift = Mods { shift: true, ..Mods::default() };
    rt.wheel_event(WheelEvent::new(0.0, -40.0).at(x, y).mods(shift));
    rt.wheel_event(WheelEvent::new(0.0, -40.0).at(x, y).mods(shift));
    rt.frame(0.0);
    rt.frame(0.0);
    assert_eq!(scrolled(&mut rt, &key), [80.0, 0.0], "both notches moved it right, not down");
    rt.frame(1.0);
    rt.wheel_event(WheelEvent::new(0.0, 30.0).at(x, y).mods(shift));
    rt.frame(0.0);
    rt.frame(0.0);
    assert_eq!(scrolled(&mut rt, &key), [50.0, 0.0], "and back left");
    // A trackpad with Shift held scrolls as it moves (AppKit doesn't turn it either).
    rt.frame(1.0);
    rt.wheel_event(WheelEvent::new(0.0, -30.0).at(x, y).mods(shift).precise(true));
    rt.frame(0.0);
    rt.frame(0.0);
    let at = scrolled(&mut rt, &key);
    assert!(at[0] == 50.0 && at[1] > 0.0, "{at:?}");
    let _ = std::fs::remove_dir_all(&d);
}

// ---- Drag-select (the real app's runtime) -----------------------------------------------------

/// A folder of `n` files `f00`…, for drag-select.
fn many(name: &str, n: usize) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-band-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..n {
        std::fs::write(d.join(format!("f{i:02}.txt")), "x").unwrap();
    }
    d
}

/// The laid-out rect of the element keyed `key`, if it's there.
fn rect_of(rt: &Runtime<State>, key: &str) -> Option<[f32; 4]> {
    rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key) && !e.exiting).map(|e| e.rect)
}

/// The names selected in the active pane.
fn picked(rt: &Runtime<State>) -> Vec<String> {
    let f = rt.state.cur();
    (0..f.visible.len()).filter(|&i| f.is_selected(i)).map(|i| f.entries[f.visible[i] as usize].name.clone()).collect()
}

/// The row named `name`'s bounds (x, y, w, h).
fn row_box(rt: &mut Runtime<State>, name: &str) -> [f32; 4] {
    let tree = rt.accessibility_json();
    let row = find_by(&tree, &|v| v["role"] == "list-box-option" && v["name"].as_str().is_some_and(|n| n.starts_with(name)))
        .unwrap_or_else(|| panic!("a row named {name}"));
    let b: Vec<f32> = row["bounds"].as_array().expect("bounds").iter().map(|x| x.as_f64().unwrap() as f32).collect();
    [b[0], b[1], b[2], b[3]]
}

/// Presses at `from`, moves through `path` (a frame after each), and lets go at the last point.
fn drag_through(rt: &mut Runtime<State>, from: (f32, f32), path: &[(f32, f32)]) {
    rt.pointer_move(from.0, from.1);
    rt.pointer_down(from.0, from.1);
    rt.frame(0.0);
    for &(x, y) in path {
        rt.pointer_move(x, y);
        rt.frame(0.0);
    }
    let end = path.last().copied().unwrap_or(from);
    rt.pointer_up(end.0, end.1);
    rt.frame(0.0);
    rt.frame(0.0);
}

fn names(r: std::ops::RangeInclusive<usize>) -> Vec<String> {
    r.map(|i| format!("f{i:02}.txt")).collect()
}

/// A press on an unselected row's empty part (its Date cell), dragged down over four rows,
/// selects them live and draws the rectangle; letting go keeps them (the release isn't a click
/// that selects one row). From the empty space below the rows, dragged up, the same.
#[test]
fn a_rectangle_from_empty_space_selects_the_rows_it_touches() {
    let d = many("list", 12);
    let mut rt = runtime(&d);
    let a = row_box(&mut rt, "f03");
    let start = (a[0] + a[2] - 30.0, a[1] + a[3] * 0.5);
    rt.pointer_move(start.0, start.1);
    rt.pointer_down(start.0, start.1);
    rt.frame(0.0);
    for k in 1..=4 {
        rt.pointer_move(start.0 - 10.0 * k as f32, start.1 + a[3] * k as f32);
        rt.frame(0.0);
    }
    assert_eq!(picked(&rt), names(3..=7), "live, as it moves");
    let band = rect_of(&rt, "band").expect("the rectangle is drawn");
    assert!(band[3] >= a[3] * 4.0 - 1.0 && band[1] <= start.1, "{band:?}");
    assert_eq!(rt.state.cur().selected().map(|e| e.name.as_str()), Some("f07.txt"), "the cursor at the pointer's end");
    rt.pointer_up(start.0 - 40.0, start.1 + a[3] * 4.0);
    rt.frame(0.0);
    rt.frame(0.0);
    assert_eq!(picked(&rt), names(3..=7), "the release keeps them");
    assert!(rect_of(&rt, "band").is_none(), "no rectangle after");
    // A click (no move) on a row's empty part selects that row, as before.
    let b = row_box(&mut rt, "f09");
    drag_through(&mut rt, (b[0] + b[2] - 30.0, b[1] + b[3] * 0.5), &[]);
    assert_eq!(picked(&rt), names(9..=9));
    // Below the last row, dragged up to f10.
    let last = row_box(&mut rt, "f11");
    let below = (last[0] + 200.0, last[1] + last[3] * 3.0);
    let up = row_box(&mut rt, "f10");
    drag_through(&mut rt, below, &[(below.0, below.1 - 20.0), (below.0 - 50.0, up[1] + 5.0)]);
    assert_eq!(picked(&rt), names(10..=11));
    // A click on one of the rows it selected moves the cursor there (the rectangle's release
    // doesn't swallow it), keeping the selection, as a click in a multiple selection does.
    assert_eq!(rt.state.cur().selected().map(|e| e.name.as_str()), Some("f10.txt"), "the cursor at the pointer's end");
    let eleven = row_box(&mut rt, "f11");
    drag_through(&mut rt, (eleven[0] + eleven[2] - 30.0, eleven[1] + eleven[3] * 0.5), &[]);
    assert_eq!(rt.state.cur().selected().map(|e| e.name.as_str()), Some("f11.txt"));
    assert_eq!(picked(&rt), names(10..=11));
    // A click below the rows clears a multiple selection (the cursor stays).
    drag_through(&mut rt, below, &[]);
    assert!(rt.state.cur().marked.is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

/// ⇧ adds the rows touched to the selection the press started with; ⌘ (Ctrl) toggles them.
#[test]
fn shift_adds_and_cmd_toggles() {
    let d = many("mods", 12);
    let mut rt = runtime(&d);
    rt.update(|s, _| {
        s.cur_mut().select(0);
        s.cur_mut().toggle_mark(4);
    });
    rt.frame(0.0);
    assert_eq!(picked(&rt), ["f00.txt", "f04.txt"]);
    let a = row_box(&mut rt, "f05");
    let start = (a[0] + a[2] - 30.0, a[1] + a[3] * 0.5);
    let mods = |shift, cmd| rune::PhysicalInput::Modifiers(Mods { shift, cmd, ctrl: cmd && !cfg!(target_os = "macos"), ..Mods::default() });
    rt.physical_input(mods(true, false));
    drag_through(&mut rt, start, &[(start.0, start.1 + 10.0), (start.0, start.1 + a[3])]);
    assert_eq!(picked(&rt), ["f00.txt", "f04.txt", "f05.txt", "f06.txt"]);
    // ⌘ from f04's row (selected, so its empty part drags files: start just below, in f07)
    // over f06 and f07: f06 goes, f07 joins.
    let b = row_box(&mut rt, "f07");
    let start = (b[0] + b[2] - 30.0, b[1] + b[3] * 0.5);
    rt.physical_input(mods(false, true));
    drag_through(&mut rt, start, &[(start.0, start.1 - 10.0), (start.0, start.1 - b[3])]);
    rt.physical_input(mods(false, false));
    assert_eq!(picked(&rt), ["f00.txt", "f04.txt", "f05.txt", "f07.txt"]);
    let _ = std::fs::remove_dir_all(&d);
}

/// Dragged below the list's bottom edge, it scrolls a step on each move and keeps selecting
/// what scrolls into it (rows that were never built: hits come from geometry).
#[test]
fn past_the_edge_it_scrolls_on_each_move() {
    let d = many("scroll", 80);
    let mut rt = runtime(&d);
    let a = row_box(&mut rt, "f02");
    let start = (a[0] + a[2] - 30.0, a[1] + a[3] * 0.5);
    let list = rect_of(&rt, "list-0").unwrap();
    let outside = list[1] + list[3] + 60.0;
    rt.pointer_move(start.0, start.1);
    rt.pointer_down(start.0, start.1);
    rt.frame(0.0);
    let mut path = vec![(start.0, start.1 + 20.0)];
    // Twenty moves out past the bottom edge (a pixel apart: still the same distance out).
    path.extend((0..20).map(|k| (start.0 + k as f32, outside)));
    for &(x, y) in &path {
        rt.pointer_move(x, y);
        rt.frame(0.0);
    }
    let scrolled = rt.state.cur().scroll.get();
    assert!(scrolled > 400.0, "20 moves × 30 pt: {scrolled}");
    let got = picked(&rt);
    assert_eq!(got.first().map(String::as_str), Some("f02.txt"));
    let last: usize = got.last().unwrap()[1..3].parse().unwrap();
    assert!(last > 40 && got.len() == last - 1, "contiguous, down past the rows first built: {got:?}");
    // Holding still sends nothing, so nothing scrolls (no timer).
    rt.frame(0.0);
    rt.frame(0.0);
    assert_eq!(rt.state.cur().scroll.get(), scrolled);
    rt.pointer_up(path.last().unwrap().0, outside);
    rt.frame(0.0);
    let _ = std::fs::remove_dir_all(&d);
}

/// Pressing on an unselected row's name, or anywhere on a selected row, and dragging, is a file
/// drag, never a rectangle; and a file drag nothing took still leaves the keys working.
#[test]
fn names_and_selected_rows_still_drag_files() {
    let d = many("files", 12);
    let mut rt = runtime(&d);
    let a = row_box(&mut rt, "f04");
    // The name's text starts about 22 pt in (padding and the glyph).
    let on_name = (a[0] + 30.0, a[1] + a[3] * 0.5);
    rt.pointer_move(on_name.0, on_name.1);
    rt.pointer_down(on_name.0, on_name.1);
    rt.frame(0.0);
    rt.pointer_move(on_name.0 + 3.0, on_name.1 + 30.0);
    rt.frame(0.0);
    rt.pointer_move(on_name.0 + 5.0, on_name.1 + 60.0);
    rt.frame(0.0);
    assert!(rt.state.band.is_none() && rect_of(&rt, "band").is_none(), "a file drag, not a rectangle");
    rt.pointer_up(on_name.0 + 5.0, on_name.1 + 60.0);
    rt.frame(0.0);
    rt.frame(0.0);
    // Select f06, then drag it from its empty part: still a file drag.
    let b = row_box(&mut rt, "f06");
    let empty = (b[0] + b[2] - 30.0, b[1] + b[3] * 0.5);
    drag_through(&mut rt, empty, &[]);
    assert_eq!(picked(&rt), names(6..=6));
    rt.pointer_move(empty.0, empty.1);
    rt.pointer_down(empty.0, empty.1);
    rt.frame(0.0);
    rt.pointer_move(empty.0, empty.1 + 40.0);
    rt.frame(0.0);
    assert!(rt.state.band.is_none(), "a selected row drags its files");
    rt.pointer_move(empty.0, empty.1 + 70.0);
    rt.pointer_up(empty.0, empty.1 + 70.0);
    rt.frame(0.0);
    rt.frame(0.0);
    assert_eq!(picked(&rt), names(6..=6), "the selection is left alone");
    key(&mut rt, Key::Function(2), false);
    assert!(rt.state.rename.is_some(), "keys work right after");
    let _ = std::fs::remove_dir_all(&d);
}

// ---- The icon view (the real app's runtime) ---------------------------------------------------

/// Switches the active tab to the icon view and lets it lay out.
fn icons(rt: &mut Runtime<State>) {
    run(rt, Cmd::AsIcons);
    rt.frame(0.0);
    rt.frame(0.0);
}

/// The grid's cells are options of the list box, named by their names, each with its place in
/// the folder (rows are virtual, so Rune can't count them), the selected one selected; ↓ moves a
/// row, → a cell, and the list's active descendant follows.
#[test]
fn the_grid_is_a_list_box_of_named_options() {
    let d = many("grid-tree", 10);
    let mut rt = runtime(&d);
    icons(&mut rt);
    let tree = rt.accessibility_json();
    let list = find(&tree, "list-0").expect("the list");
    println!("{}", outline(list));
    assert_eq!(list["role"], "list-box");
    let mut opts = Vec::new();
    all(list, "list-box-option", &mut opts);
    assert_eq!(opts.len(), 10, "{}", outline(list));
    assert_eq!(opts[2]["name"], "f02.txt");
    assert_eq!(opts[2]["position"].as_u64(), Some(3));
    assert_eq!(find(list, "rows-0").unwrap()["setSize"].as_u64(), Some(10), "the items, not the grid's rows");
    assert!(states(opts[0]).contains(&"selected") && !states(opts[1]).contains(&"selected"));
    let cols = rt.state.cur().grid_cols.get();
    assert!(cols >= 4, "a 980 pt window holds several columns: {cols}");
    key(&mut rt, Key::Down, false);
    assert_eq!(rt.state.cur().sel, cols, "↓ moves a row");
    key(&mut rt, Key::Right, false);
    assert_eq!(rt.state.cur().sel, cols + 1, "→ moves a cell (not into the folder)");
    key(&mut rt, Key::Left, true);
    assert_eq!(picked(&rt), names(cols..=cols + 1), "⇧← extends");
    key(&mut rt, Key::Up, false);
    assert_eq!(rt.state.cur().sel, 0);
    let tree = rt.accessibility_json();
    assert!(find(&tree, "list-0").unwrap()["activeDescendant"].as_str().is_some_and(|a| a.ends_with("f:f00.txt")));
    // Typing filters, F2 renames under the icon, Esc leaves the rename.
    rt.key(KeyEvent { key: Key::Text("f05".into()), mods: Mods::default() });
    rt.frame(0.0);
    assert_eq!(rt.state.cur().visible.len(), 1);
    key(&mut rt, Key::Function(2), false);
    assert!(rt.state.rename.is_some() && rect_of(&rt, "rename").is_some(), "the rename field is in the cell");
    for _ in 0..3 {
        if rt.state.rename.is_some() {
            key(&mut rt, Key::Escape, false);
        }
    }
    assert!(rt.state.rename.is_none());
    key(&mut rt, Key::Escape, false);
    assert_eq!(rt.state.cur().visible.len(), 10, "Esc clears the filter");
    let tree = rt.accessibility_json();
    // The view switch in the path bar: radio buttons, the tab's own checked.
    let icons_button = find(&tree, "view-icons").unwrap();
    assert_eq!((icons_button["role"].as_str(), icons_button["name"].as_str()), (Some("radio-button"), Some("As Icons")));
    assert!(states(icons_button).contains(&"checked") && !states(find(&tree, "view-list").unwrap()).contains(&"checked"));
    let _ = std::fs::remove_dir_all(&d);
}

/// The view is the tab's: a new tab gets the default (`view.default`), and showing a tab again
/// brings its own back. Icon sizes change with ⌘+ and ⌘- and only in the icon view.
#[test]
fn the_view_is_remembered_per_tab_and_new_tabs_take_the_default() {
    use crate::grid::{IconSize, ViewMode};
    let d = many("grid-tabs", 4);
    let mut rt = runtime(&d);
    assert_eq!(rt.state.unavailable(Cmd::LargerIcons), Some("Icon sizes are for the icon view (As Icons)"));
    icons(&mut rt);
    run(&mut rt, Cmd::NewTab);
    assert_eq!(rt.state.cur().view, ViewMode::List, "a new tab takes the default");
    run(&mut rt, Cmd::PrevTab);
    assert_eq!(rt.state.cur().view, ViewMode::Icons, "the first tab kept its own");
    assert_eq!(rt.state.panes[0].tabs[1].as_ref().map(|m| m.view), Some(ViewMode::List), "in the hidden tab's memo");
    rt.update(|s, _| s.settings.view_default = ViewMode::Icons);
    run(&mut rt, Cmd::NewTab);
    assert_eq!(rt.state.cur().view, ViewMode::Icons, "view.default = \"icons\"");
    // ⌘+ and ⌘-, the columns for the new size known at once (to reveal the cursor's row).
    assert_eq!(rt.state.icon_size, IconSize::Medium);
    let medium = rt.state.cur().grid_cols.get();
    run(&mut rt, Cmd::LargerIcons);
    assert_eq!(rt.state.icon_size, IconSize::Large);
    assert!(rt.state.cur().grid_cols.get() < medium, "fewer, larger columns");
    assert!(rt.state.unavailable(Cmd::LargerIcons).is_some(), "as large as they go");
    run(&mut rt, Cmd::SmallerIcons);
    run(&mut rt, Cmd::SmallerIcons);
    assert_eq!(rt.state.icon_size, IconSize::Small);
    // A search keeps the view it has when it ends.
    run(&mut rt, Cmd::AsList);
    let _ = std::fs::remove_dir_all(&d);
}

/// A rectangle in the grid's gutter selects the cells it touches, from geometry.
#[test]
fn a_rectangle_in_the_grid_selects_cells() {
    let d = many("grid-band", 20);
    let mut rt = runtime(&d);
    icons(&mut rt);
    let cols = rt.state.cur().grid_cols.get();
    let list = rect_of(&rt, "list-0").unwrap();
    let g = crate::grid::Geom { cols, cell: crate::grid::IconSize::Medium.cell() };
    // From the gutter left of cell 1 (row 0) to the middle of cell cols + 2 (row 1).
    let a = g.item_rect(1);
    let b = g.item_rect(cols + 2);
    let from = (list[0] + a[0] - 3.0, list[1] + a[1] + 10.0);
    let to = (list[0] + b[0] + b[2] * 0.5, list[1] + b[1] + b[3] * 0.5);
    drag_through(&mut rt, from, &[(from.0 + 4.0, from.1 + 8.0), to]);
    let want: Vec<String> = [1, 2, cols + 1, cols + 2].iter().map(|i| format!("f{i:02}.txt")).collect();
    assert_eq!(picked(&rt), want);
    let _ = std::fs::remove_dir_all(&d);
}

// ---- Thumbnails --------------------------------------------------------------------------------

/// A folder of `n` "images" decoded by the test stand-in (`tiles::hook`).
fn images(name: &str, n: usize) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-tiles-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..n {
        std::fs::write(d.join(format!("i{i:03}.png")), "px").unwrap();
    }
    crate::tiles::hook::FAKE_IN.lock().unwrap().push(d.clone());
    d
}

fn seen_in(d: &Path) -> Vec<PathBuf> {
    crate::tiles::hook::SEEN.lock().unwrap().iter().filter(|p| p.starts_with(d)).cloned().collect()
}

/// Frames until `done` or 5 s.
fn until(rt: &mut Runtime<State>, done: impl Fn(&State) -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done(&rt.state) && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(2));
        rt.frame(0.0);
    }
}

/// Only the cells on screen are thumbnailed, by at most two workers; cells show their icon
/// until theirs lands. The list view, and a folder with nothing to thumbnail, start no workers.
#[test]
fn only_visible_cells_are_thumbnailed() {
    let d = images("visible", 300);
    let mut rt = runtime(&d);
    rt.frame(0.0);
    assert!(rt.state.tile_pool.is_none(), "the list view decodes nothing");
    icons(&mut rt);
    let items = rt.state.cur().tiles.as_ref().expect("the grid asked").items.clone();
    assert!(items.start == 0 && items.end > 6 && items.end < 60, "the cells on screen: {items:?}");
    until(&mut rt, |s| s.cur().tiles.as_ref().is_some_and(|t| t.shown.len() == items.len()));
    let t = rt.state.cur().tiles.as_ref().unwrap();
    assert_eq!(t.shown.len(), items.len(), "each visible cell's thumbnail landed");
    let seen = seen_in(&d);
    assert_eq!(seen.len(), items.len(), "nothing off screen was decoded");
    assert!(seen.iter().all(|p| p.file_name().unwrap().to_str().unwrap()[1..4].parse::<usize>().unwrap() < items.end));
    let (waiting, running, most, _) = rt.state.tile_pool.as_ref().unwrap().stats();
    assert_eq!((waiting, running), (0, 0), "finished and stopped");
    assert!(most <= crate::tiles::WORKERS, "at most {} at once: {most}", crate::tiles::WORKERS);
    // Back to the list: the thumbnails go.
    run(&mut rt, Cmd::AsList);
    assert!(rt.state.cur().tiles.is_none());
    // A folder without images: no requests.
    let plain = many("no-images", 30);
    rt.update(move |s, _| s.go(plain, crate::app::Nav::New, None));
    until(&mut rt, |s| !s.cur().is_loading());
    icons(&mut rt);
    assert!(rt.state.cur().tiles.is_none(), "nothing to thumbnail, nothing kept");
    let _ = std::fs::remove_dir_all(&d);
}

/// Scrolling away cancels what's waiting or being decoded for the cells that left (a decode
/// held at its start sees its stop flag) and drops the thumbnails they had; scrolling back
/// decodes them again (nothing kept). Leaving the folder drops everything.
#[test]
fn scrolling_cancels_and_drops_what_left_the_screen() {
    let d = images("scroll", 400);
    // The first two cells' decodes wait at a gate that's never opened: only a stop ends them.
    let mut gates = Vec::new();
    for i in 0..2 {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        crate::tiles::hook::GATES.lock().unwrap().push((d.join(format!("i{i:03}.png")), wait));
        gates.push(go);
    }
    let mut rt = runtime(&d);
    icons(&mut rt);
    let first = rt.state.cur().tiles.as_ref().unwrap().items.clone();
    // Both workers are held: everything else on screen waits in the queue.
    until(&mut rt, |s| s.tile_pool.as_ref().is_some_and(|p| p.stats().1 == 2));
    let (waiting, ..) = rt.state.tile_pool.as_ref().unwrap().stats();
    assert_eq!(waiting, first.len() - 2);
    // Scroll far down.
    let list = rect_of(&rt, "list-0").unwrap();
    for _ in 0..6 {
        rt.wheel(list[0] + 100.0, list[1] + 100.0, -1500.0);
        rt.frame(0.0);
    }
    rt.frame(0.0);
    let now = rt.state.cur().tiles.as_ref().unwrap().items.clone();
    assert!(now.start > first.end, "{first:?} → {now:?}");
    until(&mut rt, |s| s.tile_pool.as_ref().is_some_and(|p| p.stats().1 == 0 && p.stats().0 == 0));
    until(&mut rt, |s| s.cur().tiles.as_ref().is_some_and(|t| t.shown.len() == now.len()));
    let t = rt.state.cur().tiles.as_ref().unwrap();
    assert!(t.shown.len() == now.len() && t.waiting() == 0);
    let seen = seen_in(&d);
    let idx = |p: &PathBuf| p.file_name().unwrap().to_str().unwrap()[1..4].parse::<usize>().unwrap();
    // The first screen's queued cells never started: only the two held ones, then the new ones.
    assert_eq!(seen.iter().filter(|p| idx(p) < first.end).count(), 2, "{seen:?}");
    assert!(
        t.shown.keys().all(|k| {
            let i: usize = k.to_str().unwrap()[1..4].parse().unwrap();
            now.contains(&i)
        }),
        "only what's on screen is kept"
    );
    drop(gates);
    // Back to the top: decoded again.
    for _ in 0..12 {
        rt.wheel(list[0] + 100.0, list[1] + 100.0, 1500.0);
        rt.frame(0.0);
    }
    until(&mut rt, |s| s.cur().tiles.as_ref().is_some_and(|t| t.shown.len() == first.len()));
    let again = seen_in(&d).into_iter().filter(|p| idx(p) >= 2 && idx(p) < first.end).count();
    assert_eq!(again, first.len() - 2, "scrolling back decodes again");
    // Leaving the folder drops it all.
    let parent = d.parent().unwrap().to_path_buf();
    rt.update(move |s, _| s.go(parent, crate::app::Nav::New, None));
    assert!(rt.state.cur().tiles.is_none());
    let _ = std::fs::remove_dir_all(&d);
}

/// Thumbnails live only while their grid is on screen: a new tab in the list view, a filter
/// that leaves nothing, the preview covering the pane of a split window, and another tab shown
/// each drop every decoded thumbnail and stop what's queued or decoding, at once, without
/// waiting for the vanishing grid to say so.
#[test]
fn thumbnails_go_with_the_grid_whatever_hides_it() {
    let held = |d: &Path| {
        let mut gates = Vec::new();
        for i in 0..2 {
            let (go, wait) = std::sync::mpsc::channel::<()>();
            crate::tiles::hook::GATES.lock().unwrap().push((d.join(format!("i{i:03}.png")), wait));
            gates.push(go);
        }
        gates
    };
    let idle = |rt: &mut Runtime<State>| {
        until(rt, |s| s.tile_pool.as_ref().is_some_and(|p| p.stats().0 == 0 && p.stats().1 == 0));
        let (waiting, running, ..) = rt.state.tile_pool.as_ref().unwrap().stats();
        assert_eq!((waiting, running), (0, 0), "nothing queued or decoding");
    };
    // A new tab (list view by default): the hidden tab's grid goes, with its held decodes.
    let d = images("gone-tab", 60);
    let _gates = held(&d);
    let mut rt = runtime(&d);
    icons(&mut rt);
    until(&mut rt, |s| s.tile_pool.as_ref().is_some_and(|p| p.stats().1 == 2));
    run(&mut rt, Cmd::NewTab);
    assert!(rt.state.cur().tiles.is_none(), "a list-view tab keeps no thumbnails");
    idle(&mut rt);
    // Showing the grid's tab again asks afresh; showing the other tab drops it again.
    run(&mut rt, Cmd::PrevTab);
    assert_eq!(rt.state.cur().view, crate::grid::ViewMode::Icons);
    until(&mut rt, |s| s.cur().tiles.as_ref().is_some_and(|t| !t.shown.is_empty()));
    run(&mut rt, Cmd::NextTab);
    assert!(rt.state.cur().tiles.is_none());
    // A filter that leaves nothing.
    run(&mut rt, Cmd::PrevTab);
    until(&mut rt, |s| s.cur().tiles.as_ref().is_some_and(|t| !t.shown.is_empty()));
    rt.key(KeyEvent { key: Key::Text("zzz".into()), mods: Mods::default() });
    rt.frame(0.0);
    assert!(rt.state.cur().visible.is_empty());
    assert!(rt.state.cur().tiles.is_none(), "no cells, no thumbnails");
    key(&mut rt, Key::Escape, false);
    // The preview covering the grid's pane in a split window.
    let d2 = images("gone-covered", 60);
    let mut rt = runtime(&d2);
    icons(&mut rt);
    until(&mut rt, |s| s.cur().tiles.as_ref().is_some_and(|t| !t.shown.is_empty()));
    run(&mut rt, Cmd::Split);
    run(&mut rt, Cmd::TogglePreview);
    let covered = 1 - rt.state.active;
    assert!(rt.state.panes[covered].f.tiles.is_none(), "a covered grid keeps no thumbnails");
    idle(&mut rt);
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_dir_all(&d2);
}

/// A reload, or a new icon size, keeps the grid's rows (and their keys) as they were, so Rune
/// reports no change in what's visible: the thumbnails are asked for again anyway, from the
/// rows last seen. Filtered to one image and reloaded, its thumbnail comes back; on a
/// one-image grid a new size decodes it again at that size.
#[test]
fn thumbnails_come_back_after_a_reload_and_follow_the_icon_size() {
    let d = images("again", 30);
    let mut rt = runtime(&d);
    icons(&mut rt);
    rt.key(KeyEvent { key: Key::Text("i017".into()), mods: Mods::default() });
    rt.frame(0.0);
    rt.frame(0.0);
    assert_eq!(rt.state.cur().visible.len(), 1);
    until(&mut rt, |s| s.cur().tiles.as_ref().is_some_and(|t| t.shown.len() == 1));
    run(&mut rt, Cmd::Reload);
    until(&mut rt, |s| !s.cur().is_loading() && s.cur().tiles.as_ref().is_some_and(|t| t.shown.len() == 1));
    assert!(rt.state.cur().tiles.as_ref().is_some_and(|t| t.shown.len() == 1), "the thumbnail is back after the reload");
    let one = d.join("i017.png");
    let decoded = |d: &Path| seen_in(d).iter().filter(|p| **p == one).count();
    let before = decoded(&d);
    run(&mut rt, Cmd::LargerIcons);
    until(&mut rt, |s| s.cur().tiles.as_ref().is_some_and(|t| t.shown.len() == 1) && decoded(&d) > before);
    assert!(decoded(&d) > before, "decoded again for the new size");
    let fit = (crate::grid::IconSize::Large.icon() * 1.0).round() as u32;
    assert!(rt.state.cur().tiles.as_ref().is_some_and(|t| t.fit() == fit), "at the large size's pixels");
    let _ = std::fs::remove_dir_all(&d);
}

/// The wheel scrolling the list under a held rectangle: its far corner stays under the pointer
/// in the content, so it grows over the rows scrolled into it, and they're selected, with no
/// pointer move and no timer.
#[test]
fn the_wheel_under_a_held_rectangle_updates_it() {
    let d = many("wheel-band", 100);
    let mut rt = runtime(&d);
    let a = row_box(&mut rt, "f03");
    let start = (a[0] + a[2] - 30.0, a[1] + a[3] * 0.5);
    rt.pointer_move(start.0, start.1);
    rt.pointer_down(start.0, start.1);
    rt.frame(0.0);
    rt.pointer_move(start.0 - 20.0, start.1 + a[3] * 2.0);
    rt.frame(0.0);
    assert_eq!(picked(&rt), names(3..=5));
    rt.wheel(start.0, start.1, -240.0);
    rt.frame(0.0);
    rt.frame(0.0);
    let scrolled = rt.state.cur().scroll.get();
    assert!(scrolled > 200.0, "{scrolled}");
    let rows = (scrolled / a[3]).round() as usize;
    assert_eq!(picked(&rt), names(3..=5 + rows), "the rows scrolled into it");
    let band = rect_of(&rt, "band").expect("drawn");
    let list = rect_of(&rt, "list-0").unwrap();
    assert_eq!(band[1], list[1], "its start went up with the content, out of view: clipped at the top");
    rt.pointer_up(start.0 - 20.0, start.1 + a[3] * 2.0);
    rt.frame(0.0);
    assert_eq!(picked(&rt), names(3..=5 + rows));
    let _ = std::fs::remove_dir_all(&d);
}

/// Idle means idle (NORTHSTAR): once a switch of view, a resize (the window taking its saved
/// size after the first frame) or a scroll has landed, the window asks for no frame, commits
/// nothing and `poll` reports no change, in the list and the icon view, with or without
/// thumbnails. A layout report whose handler said `Tx::live_only` and changed nothing left Rune
/// drawing every frame (at 60 or 120 Hz, until the next key) in a window with no live values,
/// which is the list view (RUNE-GAPS.md).
#[test]
fn an_idle_window_draws_nothing() {
    let quiet = |rt: &mut Runtime<State>, what: &str| {
        // Let what was asked land: listings, thumbnails, scrolls.
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            rt.frame(1.0 / 60.0);
        }
        let commits = rune::commit_generation();
        for i in 0..20 {
            assert!(!rt.update_if(|s, tx| s.poll(tx)), "{what}: idle poll {i} reported a change");
            assert!(!rt.frame(1.0 / 60.0), "{what}: idle frame {i} is animating");
            assert!(!rt.live_pending(), "{what}: idle frame {i} asks for another frame");
        }
        assert_eq!(rune::commit_generation(), commits, "{what}: idle frames committed");
    };
    let shake = |rt: &mut Runtime<State>, what: &str| {
        quiet(rt, what);
        rt.set_viewport((1100.0, 700.0), 1.0);
        quiet(rt, &format!("{what}, resized"));
        rt.set_viewport((980.0, 620.0), 1.0);
        quiet(rt, &format!("{what}, resized back"));
        rt.wheel(500.0, 300.0, 120.0);
        quiet(rt, &format!("{what}, scrolled"));
    };
    for (name, d) in [("plain", many("idle", 80)), ("images", images("idle", 80))] {
        let mut rt = runtime(&d);
        shake(&mut rt, &format!("{name}: list"));
        run(&mut rt, Cmd::AsIcons);
        shake(&mut rt, &format!("{name}: icons"));
        run(&mut rt, Cmd::AsList);
        shake(&mut rt, &format!("{name}: list again"));
        run(&mut rt, Cmd::NewTab);
        shake(&mut rt, &format!("{name}: new tab"));
        let _ = std::fs::remove_dir_all(&d);
    }
}

// ---- The column view and the tiles view (the real app's runtime) ------------------------------

/// d/{Docs/{Work/{notes.md, plan.txt}, readme.txt}, Photos/, top.txt}.
fn nested(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-a11y-cols-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("Docs/Work")).unwrap();
    std::fs::create_dir_all(d.join("Photos")).unwrap();
    std::fs::write(d.join("Docs/Work/notes.md"), "# Notes\n").unwrap();
    std::fs::write(d.join("Docs/Work/plan.txt"), "plan").unwrap();
    std::fs::write(d.join("Docs/readme.txt"), "read me").unwrap();
    std::fs::write(d.join("top.txt"), "top").unwrap();
    d
}

/// Frames until no column (or the preview) is loading.
fn settle_columns(rt: &mut Runtime<State>) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let busy = |s: &State| {
        s.cur().is_loading()
            || s.preview.as_ref().is_some_and(|p| p.is_loading())
            || s.cur().cols.as_ref().is_some_and(|c| c.left.iter().chain(c.right.as_ref()).any(|c| c.load == crate::columns::Load::Loading))
    };
    while busy(&rt.state) && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(3));
        rt.frame(0.0);
    }
    rt.frame(0.0);
    rt.frame(0.0);
}

/// The column view is a group of list boxes, one per column, each named after its folder; in a
/// column left of the active one the folder on the way is the selected option; the active
/// column is the focusable list with the cursor as its active descendant; a file's preview is
/// the last column. → and ← move between columns, and a click in another column makes it the
/// active one with the item clicked.
#[test]
fn the_columns_are_a_group_of_named_list_boxes() {
    let d = nested("tree");
    let top = crate::app::title(&d);
    let mut rt = runtime(&d);
    // Narrower columns, so all three and the preview fit the 980 pt window.
    rt.update(|s, _| s.col_w = 160.0);
    run(&mut rt, Cmd::AsColumns);
    settle_columns(&mut rt);
    key(&mut rt, Key::Right, false);
    settle_columns(&mut rt);
    key(&mut rt, Key::Right, false);
    settle_columns(&mut rt);
    assert_eq!(rt.state.cur().cwd, d.join("Docs/Work"));
    assert_eq!(rt.state.cur().selected().map(|e| e.name.as_str()), Some("notes.md"));
    let tree = rt.accessibility_json();
    let group = find(&tree, "columns-0").expect("the columns");
    println!("{}", outline(group));
    assert_eq!((group["role"].as_str(), group["name"].as_str()), (Some("group"), Some("Columns, 3 folders")));
    let mut boxes = Vec::new();
    all(group, "list-box", &mut boxes);
    let names: Vec<&str> = boxes.iter().filter_map(|b| b["name"].as_str()).collect();
    assert_eq!(names, [format!("Files in {top}").as_str(), "Files in Docs", "Files in Work"]);
    let on_the_way: Vec<String> = boxes[..2]
        .iter()
        .map(|b| {
            let mut opts = Vec::new();
            all(b, "list-box-option", &mut opts);
            let sel: Vec<&str> = opts.iter().filter(|o| states(o).contains(&"selected")).filter_map(|o| o["name"].as_str()).collect();
            sel.join(",")
        })
        .collect();
    assert_eq!(on_the_way, ["Folder Docs", "Folder Work"], "the ▸ reads as Folder, as without the Kind column");
    assert!(find(group, "columns-back").is_none(), "nothing scrolled off");
    assert_eq!(boxes[2]["key"], "list-0", "the active column is the pane's list");
    assert!(boxes[2]["activeDescendant"].as_str().is_some_and(|a| a.ends_with("f:notes.md")));
    let preview = find(group, "preview").expect("the preview is the last column");
    assert_eq!(preview["role"], "complementary");
    // The view switch: As Columns checked.
    assert!(states(find(&tree, "view-columns").unwrap()).contains(&"checked"));
    // ← goes back a column; a click on readme.txt in Docs' column makes Docs active with it.
    key(&mut rt, Key::Left, false);
    settle_columns(&mut rt);
    assert_eq!(rt.state.cur().cwd, d.join("Docs"));
    assert_eq!(rt.state.cur().selected().map(|e| e.name.as_str()), Some("Work"));
    key(&mut rt, Key::Left, false);
    settle_columns(&mut rt);
    assert_eq!(rt.state.cur().cwd, d);
    let b = row_box(&mut rt, "readme.txt");
    let at = (b[0] + 20.0, b[1] + b[3] * 0.5);
    rt.pointer_move(at.0, at.1);
    rt.pointer_down(at.0, at.1);
    rt.pointer_up(at.0, at.1);
    rt.frame(0.0);
    settle_columns(&mut rt);
    assert_eq!(rt.state.cur().cwd, d.join("Docs"), "the clicked column is active");
    assert_eq!(rt.state.cur().selected().map(|e| e.name.as_str()), Some("readme.txt"));
    assert_eq!(rt.state.preview.as_ref().and_then(|p| p.target.as_ref()).map(|t| t.path()), Some(d.join("Docs/readme.txt")));
    // Keys still work on it: F2 renames in the active column.
    key(&mut rt, Key::Function(2), false);
    assert!(rt.state.rename.is_some() && rect_of(&rt, "rename").is_some());
    key(&mut rt, Key::Escape, false);
    let _ = std::fs::remove_dir_all(&d);
}

/// The tiles view: a grid of options (icon, name, kind, size) named from their texts, each with
/// its place in the folder; ↓ moves a row, → a tile. Switching views keeps the selection.
#[test]
fn tiles_are_options_and_views_switch_keeping_the_selection() {
    use crate::grid::ViewMode;
    let d = many("tiles", 10);
    let mut rt = runtime(&d);
    key(&mut rt, Key::Down, false);
    key(&mut rt, Key::Down, false);
    run(&mut rt, Cmd::AsTiles);
    rt.frame(0.0);
    rt.frame(0.0);
    assert_eq!(rt.state.cur().view, ViewMode::Tiles);
    assert_eq!(picked(&rt), names(2..=2), "the selection stays across views");
    let tree = rt.accessibility_json();
    let list = find(&tree, "list-0").expect("the list");
    println!("{}", outline(list));
    let mut opts = Vec::new();
    all(list, "list-box-option", &mut opts);
    assert_eq!(opts.len(), 10);
    assert!(opts[2]["name"].as_str().is_some_and(|n| n.starts_with("f02.txt")), "{}", opts[2]);
    assert_eq!(opts[2]["position"].as_u64(), Some(3));
    assert!(states(opts[2]).contains(&"selected"));
    let cols = rt.state.cur().grid_cols.get();
    assert!((2..=4).contains(&cols), "wide tiles, a few to a row: {cols}");
    let tile = rect_of(&rt, "f:f02.txt").expect("the tile");
    assert!(tile[2] >= crate::grid::TILE_W - 2.0 * crate::grid::INSET - 1.0 && tile[3] < 80.0, "wide and short: {tile:?}");
    key(&mut rt, Key::Down, false);
    assert_eq!(rt.state.cur().sel, 2 + cols, "↓ moves a row");
    key(&mut rt, Key::Right, false);
    assert_eq!(rt.state.cur().sel, 3 + cols, "→ moves a tile");
    assert!(states(find(&tree, "view-tiles").unwrap()).contains(&"checked"));
    assert_eq!(rt.state.unavailable(Cmd::LargerIcons), Some("Icon sizes are for the icon view (As Icons)"));
    // Through every view and back; the cursor stays on its item.
    let want = rt.state.cur().cursor_name();
    for cmd in [Cmd::AsIcons, Cmd::AsColumns, Cmd::AsList, Cmd::AsTiles, Cmd::AsList] {
        run(&mut rt, cmd);
        rt.frame(0.0);
        assert_eq!(rt.state.cur().cursor_name(), want, "{cmd:?}");
        assert_eq!(
            rt.state.unavailable(cmd),
            Some(match cmd {
                Cmd::AsIcons => "Already showing as icons",
                Cmd::AsColumns => "Already showing as columns",
                Cmd::AsTiles => "Already showing as tiles",
                _ => "Already showing as a list",
            })
        );
    }
    assert!(rt.state.cur().cols.is_none() && rt.state.cur().tiles.is_none(), "nothing kept from the views left");
    // view.default = "columns": a new tab opens as columns.
    rt.update(|s, _| s.settings.view_default = ViewMode::Columns);
    run(&mut rt, Cmd::NewTab);
    settle_columns(&mut rt);
    assert_eq!(rt.state.cur().view, ViewMode::Columns);
    assert!(rt.state.cur().cols.is_some() && find(&rt.accessibility_json(), "columns-0").is_some());
    let _ = std::fs::remove_dir_all(&d);
}

/// A sideways wheel over a column (each column is its own scroll view, which a wheel handler
/// outside it never hears) scrolls the columns back by whole columns, reading the one that
/// comes on screen; a vertical one scrolls the column itself.
#[test]
fn a_sideways_wheel_over_a_column_scrolls_the_columns() {
    use rune::WheelEvent;
    let d = nested("wheel");
    let mut rt = runtime(&d);
    // Room for two: the active column and the preview.
    rt.update(|s, _| s.col_w = 400.0);
    run(&mut rt, Cmd::AsColumns);
    settle_columns(&mut rt);
    key(&mut rt, Key::Right, false);
    settle_columns(&mut rt);
    key(&mut rt, Key::Right, false);
    settle_columns(&mut rt);
    let shown = |rt: &Runtime<State>| rt.state.cur().cols.as_ref().map(|c| c.shown());
    assert_eq!(shown(&rt), Some(2..4), "Work and notes.md's preview");
    assert!(rt.state.cur().cols.as_ref().unwrap().holding().is_empty(), "the root and Docs are off screen");
    let list = rect_of(&rt, "list-0").expect("the active column");
    let (x, y) = (list[0] + 40.0, list[1] + 30.0);
    rt.wheel_event(WheelEvent::new(0.0, -30.0).at(x, y));
    rt.frame(0.0);
    assert_eq!(shown(&rt), Some(2..4), "a vertical wheel leaves the columns alone");
    rt.wheel_event(WheelEvent::new(120.0, 0.0).at(x, y));
    rt.frame(0.0);
    settle_columns(&mut rt);
    assert_eq!(shown(&rt), Some(1..3), "back a column, as far as the active one stays on screen");
    assert_eq!(rt.state.cur().cols.as_ref().unwrap().holding(), [d.join("Docs")], "Docs read again");
    assert!(find(&rt.accessibility_json(), "columns-back").is_some(), "and the way back to the root");
    let _ = std::fs::remove_dir_all(&d);
}

/// Idle means idle: once the columns are read and drawn, no frame asks for another (Rune's
/// `frame` says whether it's still animating), with the root's columns off screen or not, and
/// with the list focused as it moves into the columns.
#[test]
fn the_column_view_at_rest_draws_nothing() {
    let d = nested("idle");
    for f in ["a", "b", "c"] {
        std::fs::create_dir_all(d.join(f)).unwrap();
    }
    std::fs::create_dir_all(d.join("Photos/2026/Summer")).unwrap();
    let mut rt = runtime(&d);
    // No sidebar place above it: the columns start at the volume's root, as on most disks.
    rt.update(|s, _| s.places.clear());
    // The list has keyboard focus (a click or Tab put it there) when it moves into the columns.
    key(&mut rt, Key::Tab, false);
    assert_eq!(focused_key(&rt.accessibility_json()).as_deref(), Some("list-0"));
    run(&mut rt, Cmd::AsColumns);
    let mut frames = 0;
    while rt.frame(0.016) && frames < 200 {
        frames += 1;
    }
    assert!(frames < 200, "still drawing after As Columns");
    settle_columns(&mut rt);
    let mut busy = Vec::new();
    let keys =
        [Key::Right, Key::Down, Key::Down, Key::Right, Key::Left, Key::Left, Key::Down, Key::Down, Key::Down, Key::Right, Key::Right];
    let n = keys.len();
    for (step, k) in keys.clone().into_iter().chain(keys).enumerate() {
        if step == n {
            // Again with columns scrolled off to the left (and the "‹ N" button).
            rt.update(|s, _| s.col_w = 330.0);
            run(&mut rt, Cmd::Parent);
            run(&mut rt, Cmd::Parent);
        }
        key(&mut rt, k, false);
        settle_columns(&mut rt);
        // The pointer resting over the columns, as a mouse does.
        rt.pointer_move(330.0 + 37.0 * step as f32 % 600.0, 400.0);
        rt.frame(0.016);
        let mut frames = 0;
        while rt.frame(0.016) && frames < 200 {
            frames += 1;
        }
        if frames >= 200 {
            busy.push((step, rt.state.cur().cwd.clone(), rt.state.cur().cursor_name()));
        }
    }
    assert!(busy.is_empty(), "still drawing after: {busy:?}");
    let _ = std::fs::remove_dir_all(&d);
}

/// A press in a column whose read is still on its way makes that column the active one at
/// once, empty and loading, so nothing selected in another column stays the target: Trash has
/// nothing to act on (it trashed the folder selected before, review of 7b3a373). The listing
/// lands later with the item pressed selected.
#[test]
fn a_press_in_an_unread_column_leaves_nothing_else_actionable() {
    let d = nested("unread-press");
    let mut rt = runtime(&d);
    let (started, wait_started) = std::sync::mpsc::channel();
    let (go, wait_go) = std::sync::mpsc::channel();
    let (done, _) = std::sync::mpsc::channel();
    crate::fs::read_hook::GATES.lock().unwrap().push(crate::fs::read_hook::Gate {
        path: d.join("Docs"),
        started,
        go: Some(wait_go),
        done,
        seen: 0,
    });
    run(&mut rt, Cmd::AsColumns);
    wait_started.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    let c = rt.state.cur().cols.as_ref().unwrap().right.as_ref().unwrap();
    assert_eq!(c.load, crate::columns::Load::Loading);
    let b = rect_of(&rt, &c.key()).unwrap();
    let (x, y) = (b[0] + 50.0, b[1] + 100.0);
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.0);
    assert_eq!(rt.state.cur().cwd, d.join("Docs"), "the pressed column is the active folder at once");
    assert!(rt.state.cur().is_loading() && rt.state.cur().visible.is_empty());
    assert!(rt.state.target_paths().is_empty(), "nothing from the folder before stays selected");
    for cmd in [Cmd::Trash, Cmd::Delete, Cmd::Rename, Cmd::Duplicate, Cmd::Cut] {
        assert!(rt.state.unavailable(cmd).is_some(), "{cmd:?} has nothing to act on");
    }
    assert_eq!(rt.state.cur().back, std::slice::from_ref(&d), "a navigation: ⌘[ goes back");
    go.send(()).unwrap();
    until(&mut rt, |s| !s.cur().is_loading());
    settle_columns(&mut rt);
    assert_eq!(rt.state.cur().cwd, d.join("Docs"));
    assert_eq!(rt.state.cur().visible.len(), 2, "Work and readme.txt landed");
    assert!(d.join("Docs").exists());
    let _ = std::fs::remove_dir_all(&d);
}

/// Two presses in another column before a frame (Work, then readme.txt): the second selects
/// what it pressed, though its column is already the active one by then (it stayed on Work).
#[test]
fn quick_presses_in_another_column_select_the_last_one() {
    let d = nested("quick-presses");
    let mut rt = runtime(&d);
    run(&mut rt, Cmd::AsColumns);
    settle_columns(&mut rt);
    let a = row_box(&mut rt, "Folder Work");
    let b = row_box(&mut rt, "readme.txt");
    for r in [a, b] {
        let (x, y) = (r[0] + 35.0, r[1] + r[3] * 0.5);
        rt.pointer_move(x, y);
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
    }
    rt.frame(0.0);
    assert_eq!(rt.state.target_paths(), [d.join("Docs/readme.txt")]);
    let _ = std::fs::remove_dir_all(&d);
}

/// Files dropped on a column go into the folder that column listed, or nowhere: one put in its
/// place since (moved away, another made with its name) is refused on the worker, as a paste
/// into a replaced folder is, and nothing moves.
#[test]
fn a_drop_on_a_column_refuses_a_replaced_folder() {
    let d = nested("replaced-dest");
    let mut rt = runtime(&d);
    run(&mut rt, Cmd::AsColumns);
    settle_columns(&mut rt);
    let c = rt.state.cur().cols.as_ref().unwrap().right.as_ref().unwrap();
    assert!(c.dir_id.is_some(), "its identity, from its listing");
    let b = rect_of(&rt, &c.key()).unwrap();
    std::fs::rename(d.join("Docs"), d.join("Docs-away")).unwrap();
    std::fs::create_dir(d.join("Docs")).unwrap();
    rt.drop_files(vec![d.join("top.txt")], Some((b[0] + 50.0, b[1] + 150.0)));
    rt.frame(0.0);
    until(&mut rt, |s| s.job.is_none());
    assert!(d.join("top.txt").exists(), "nothing moved");
    assert!(!d.join("Docs/top.txt").exists() && !d.join("Docs-away/top.txt").exists());
    assert!(
        rt.state.note.as_deref().is_some_and(|n| n.starts_with("Nothing was dropped") && n.contains("another folder now")),
        "{:?}",
        rt.state.note
    );
    // The same drop on a column that's still its folder goes in.
    std::fs::remove_dir(d.join("Docs")).unwrap();
    std::fs::rename(d.join("Docs-away"), d.join("Docs")).unwrap();
    rt.drop_files(vec![d.join("top.txt")], Some((b[0] + 50.0, b[1] + 150.0)));
    rt.frame(0.0);
    until(&mut rt, |s| s.job.is_none());
    assert!(d.join("Docs/top.txt").exists(), "{:?}", rt.state.note);
    let _ = std::fs::remove_dir_all(&d);
}
