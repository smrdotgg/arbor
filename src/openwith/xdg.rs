#![cfg_attr(not(any(test, all(unix, not(target_os = "macos")))), allow(dead_code))]

use super::Handler;
use std::path::PathBuf;

/// The MIME type `globs2` gives `name` (its `weight:type:glob` lines): the highest weight wins,
/// then the longest pattern. Patterns match the whole name, case aside unless the line ends in
/// `:cs`.
pub fn mime_from_globs(globs2: &str, name: &str) -> Option<(u32, String)> {
    let lower = name.to_lowercase();
    // A case-sensitive match beats one that ignores case (the spec tries them in that order).
    let mut best: Option<(u32, bool, usize, String)> = None;
    for line in globs2.lines() {
        if line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(4, ':');
        let (Some(w), Some(mime), Some(glob)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Ok(w) = w.parse::<u32>() else { continue };
        let cs = parts.next().is_some_and(|f| f.split(',').any(|f| f == "cs"));
        let hit = if cs { glob_match(glob, name) } else { glob_match(&glob.to_lowercase(), &lower) };
        if !hit {
            continue;
        }
        let better = best.as_ref().is_none_or(|(bw, bcs, bl, _)| (w, cs, glob.len()) > (*bw, *bcs, *bl));
        if better {
            best = Some((w, cs, glob.len(), mime.to_string()));
        }
    }
    best.map(|(w, _, _, m)| (w, m))
}

/// A shell glob over a whole name: `*`, `?` and `[…]`.
fn glob_match(glob: &str, name: &str) -> bool {
    crate::pattern::Glob::name(glob, crate::pattern::Case::Sensitive).is_ok_and(|g| g.matches(name))
}

/// One `mimeapps.list`: its three sections, each `type=a.desktop;b.desktop;`.
#[derive(Debug, Default, PartialEq)]
pub struct MimeApps {
    pub defaults: Vec<(String, Vec<String>)>,
    pub added: Vec<(String, Vec<String>)>,
    pub removed: Vec<(String, Vec<String>)>,
}

/// Reads `[section]` → `type=id;id;` lists from a key file (mimeapps.list, mimeinfo.cache).
fn key_lists(text: &str, mut section: impl FnMut(&str) -> Option<usize>) -> Vec<Vec<(String, Vec<String>)>> {
    let mut out: Vec<Vec<(String, Vec<String>)>> = vec![Vec::new(), Vec::new(), Vec::new()];
    let mut at: Option<usize> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            at = section(name);
            continue;
        }
        let (Some(i), Some((k, v))) = (at, line.split_once('=')) else { continue };
        let ids: Vec<String> = v.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
        out[i].push((k.trim().to_string(), ids));
    }
    out
}

pub fn parse_mimeapps(text: &str) -> MimeApps {
    let mut l = key_lists(text, |s| match s {
        "Default Applications" => Some(0),
        "Added Associations" => Some(1),
        "Removed Associations" => Some(2),
        _ => None,
    })
    .into_iter();
    MimeApps { defaults: l.next().unwrap_or_default(), added: l.next().unwrap_or_default(), removed: l.next().unwrap_or_default() }
}

/// `mimeinfo.cache`'s `[MIME Cache]`: each type's apps.
pub fn parse_mimeinfo_cache(text: &str) -> Vec<(String, Vec<String>)> {
    key_lists(text, |s| (s == "MIME Cache").then_some(0)).swap_remove(0)
}

/// What Open With needs from a `.desktop` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Desktop {
    pub name: String,
    pub exec: String,
    pub mimes: Vec<String>,
    /// `Hidden=true`: deleted, as if it weren't there.
    pub hidden: bool,
}

/// The `[Desktop Entry]` of a desktop file, for an application with an `Exec` line. `lang` is
/// `$LANG` (`de_DE.UTF-8`), for a translated name.
pub fn parse_desktop(text: &str, lang: &str) -> Option<Desktop> {
    let lang = lang.split(['.', '@']).next().unwrap_or("");
    let short = lang.split('_').next().unwrap_or("");
    let (mut name, mut name_full, mut name_short) = (None, None, None);
    let (mut exec, mut mimes, mut hidden, mut app) = (None, Vec::new(), false, true);
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == "[Desktop Entry]";
            continue;
        }
        let Some((k, v)) = line.split_once('=').filter(|_| inside) else { continue };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "Name" => name = Some(v.to_string()),
            "Exec" => exec = Some(unescape_value(v)),
            "MimeType" => mimes = v.split(';').filter(|s| !s.is_empty()).map(String::from).collect(),
            "Hidden" => hidden = v == "true",
            "Type" => app = v == "Application",
            _ if !lang.is_empty() && k == format!("Name[{lang}]") => name_full = Some(v.to_string()),
            _ if !short.is_empty() && k == format!("Name[{short}]") => name_short = Some(v.to_string()),
            _ => {}
        }
    }
    let name = name_full.or(name_short).or(name)?;
    Some(Desktop { name, exec: exec.filter(|e| !e.is_empty() && app)?, mimes, hidden })
}

/// A desktop file value's escapes: `\s` `\n` `\t` `\r` `\\`.
pub fn unescape_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The command lines an `Exec` line gives for `files`: its quoting undone, `%f`/`%u` with one
/// file per run (so one run for each file), `%F`/`%U` with all of them, `%%` a percent sign, and
/// the other field codes dropped. A line with no file field gets the files at the end.
pub fn exec_argv(exec: &str, files: &[PathBuf]) -> Vec<Vec<String>> {
    // Split into arguments: "…" quotes, where \" \` \$ \\ are escapes.
    let mut args: Vec<String> = Vec::new();
    let (mut cur, mut quoted, mut any) = (String::new(), false, false);
    let mut it = exec.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            '\\' if quoted && it.peek().is_some_and(|n| matches!(n, '"' | '`' | '$' | '\\')) => cur.push(it.next().unwrap_or('\\')),
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    args.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        args.push(cur);
    }
    let paths: Vec<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let single = args.iter().any(|a| a.contains("%f") || a.contains("%u"));
    let multi = args.iter().any(|a| a == "%F" || a == "%U");
    let expand = |one: Option<&str>| -> Vec<String> {
        let mut out = Vec::new();
        for a in &args {
            if a == "%F" || a == "%U" {
                out.extend(paths.iter().cloned());
                continue;
            }
            let mut s = String::new();
            let mut ch = a.chars();
            let mut dropped = false;
            while let Some(c) = ch.next() {
                if c != '%' {
                    s.push(c);
                    continue;
                }
                match ch.next() {
                    Some('%') => s.push('%'),
                    Some('f' | 'u') => s.push_str(one.unwrap_or("")),
                    Some(_) | None => dropped = true,
                }
            }
            if !(s.is_empty() && dropped) {
                out.push(s);
            }
        }
        if !single && !multi {
            out.extend(paths.iter().cloned());
        }
        out
    };
    let runs: Vec<Vec<String>> = if single && !multi { paths.iter().map(|p| expand(Some(p))).collect() } else { vec![expand(None)] };
    runs.into_iter().filter(|r| !r.is_empty()).collect()
}

/// The apps for `mime`, best first: the first default (in the files' order) that exists and
/// isn't removed, then the added associations, then the caches'. `mimeapps` is in the spec's
/// order (the user's first); `find` reads a desktop file by id.
pub fn resolve(
    mime: &str,
    mimeapps: &[MimeApps],
    caches: &[Vec<(String, Vec<String>)>],
    find: impl Fn(&str) -> Option<Desktop>,
) -> Vec<Handler> {
    let removed: Vec<&String> =
        mimeapps.iter().flat_map(|m| m.removed.iter()).filter(|(t, _)| t == mime).flat_map(|(_, ids)| ids).collect();
    let mut out: Vec<Handler> = Vec::new();
    let add = |id: &str, default: bool, out: &mut Vec<Handler>| {
        if removed.iter().any(|r| *r == id) || out.iter().any(|h| h.id == id) {
            return false;
        }
        let Some(d) = find(id).filter(|d| !d.hidden) else { return false };
        out.push(Handler { name: d.name, id: id.to_string(), default, exec: Some(d.exec) });
        true
    };
    'defaults: for m in mimeapps {
        for (t, ids) in &m.defaults {
            if t != mime {
                continue;
            }
            for id in ids {
                if add(id, true, &mut out) {
                    break 'defaults;
                }
            }
        }
    }
    let assoc = mimeapps.iter().flat_map(|m| m.added.iter()).chain(caches.iter().flatten());
    for (t, ids) in assoc {
        if t == mime {
            for id in ids {
                add(id, false, &mut out);
            }
        }
    }
    out
}
