//! The Batch Rename dialog: its fields, switches and the preview of every item's new name.

use super::*;

/// Rows Batch Rename's preview shows before it scrolls.
pub(super) const BATCH_ROWS: f32 = 12.0;
/// The width of Batch Rename's labels ("Find", "Replace", "Number").
pub(super) const BATCH_LABEL_W: f32 = 58.0;
/// Problems in Batch Rename, and the confirm dialog's destructive button.
pub(super) fn red() -> Color {
    rune::oklch(0.60, 0.19, 27.0)
}

/// Batch Rename (`batch.rs`): the rule's fields, a live preview of every item's new name with
/// its problems, and the summary with Rename. Modal: a click outside closes it (its rule is
/// kept for next time).
pub(super) fn batch_view(s: &State, b: &crate::batch::BatchBox, l: Look) -> El {
    use crate::batch::Field;
    let field = |f: Field, edit: &rune::TextEdit, key: &'static str, name: &'static str, hint: &str, w: Option<f32>| {
        let input = text_input(edit, BODY)
            .key(key)
            .label(name)
            .single_line()
            .placeholder(hint.to_string(), l.t.text_secondary.alpha(0.6))
            .color(l.t.text)
            .caret(l.accent, l.accent.alpha(0.35))
            .on_edit(move |s: &mut State| s.batch_edit(f))
            .on_input_change(|s: &mut State, tx| {
                tx.instant();
                s.batch_replan();
            })
            .on_input_key(|s: &mut State, ev, tx| s.batch_field_key(ev, tx))
            .grow(1.0)
            .min_w(0.0);
        let r = row()
            .h(28.0)
            .px(8.0)
            .center_y()
            .radius(5.0)
            .fill(l.t.bg)
            .border(1.0, if edit.focused { l.accent } else { l.t.card_border })
            .child(input);
        match w {
            Some(w) => r.w(w),
            None => r.grow(1.0).min_w(0.0),
        }
    };
    // The fields carry their own names; these labels would only say them again.
    let label = |t: &'static str| text(t).font(SMALL).color(l.t.text_secondary).w(BATCH_LABEL_W).a11y_hidden();
    let toggle =
        |t: &'static str, key: &'static str, name: &'static str, on: bool, help: &'static str, flip: fn(&mut crate::batch::BatchBox)| {
            let r = row()
                .key(key)
                .role("switch")
                .label(name)
                .checked(on)
                .h(26.0)
                .px(8.0)
                .center()
                .radius(5.0)
                .cursor(Cursor::Pointer)
                .help(help)
                .on_press_tx(move |s: &mut State, tx| {
                    tx.instant();
                    s.batch_set(flip);
                })
                .child(text(t).font(TextStyle::new(12.0, 600.0)).color(if on { Color::WHITE } else { l.t.text_secondary }));
            if on { r.fill(l.accent) } else { r.fill(l.t.control_off).hover(l.t.hover) }
        };
    let find = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(label("Find"))
        .child(field(
            Field::Find,
            &b.find,
            "batch-find",
            "Find",
            if b.regex { "Regular expression, e.g. ^IMG_(\\d+)" } else { "Text to find (empty: replace the whole name)" },
            None,
        ))
        .child(toggle(".*", "batch-regex", "Regular expression", b.regex, "Regular expression (⌥R)", |b| b.regex = !b.regex))
        .child(toggle("Aa", "batch-case", "Match capitals", b.case_sensitive, "Match capitals (⌥C)", |b| {
            b.case_sensitive = !b.case_sensitive
        }));
    let replace = row().w_full().gap(8.0).center_y().child(label("Replace")).child(field(
        Field::Replace,
        &b.replace,
        "batch-replace",
        "Replace with",
        if b.regex { "With: text, $1, {n}, {n:3}, {name}, {ext}, {parent}" } else { "With: text, {n}, {n:3}, {name}, {ext}, {parent}" },
        None,
    ));
    let mut cases = row().key("batch-cases").role("radiogroup").label("Case").gap(2.0).px(2.0).py(2.0).radius(6.0).fill(l.t.control_off);
    for c in crate::rename::CaseMode::ALL {
        let on = b.case == c;
        let pill = row()
            .key(format!("case-{}", c.label()))
            .role("radio")
            .focus_selected(on)
            .h(22.0)
            .px(8.0)
            .center()
            .radius(4.0)
            .cursor(Cursor::Pointer)
            .on_press_tx(move |s: &mut State, tx| {
                tx.instant();
                s.batch_set(|b| b.case = c);
            })
            .child(text(c.label()).font(SMALL).color(if on { l.t.text } else { l.t.text_secondary }));
        cases = cases.child(if on { pill.fill(l.t.card).border(1.0, l.t.card_border) } else { pill.hover(l.t.hover) });
    }
    let ext_on = b.include_ext;
    let check = el().w(14.0).h(14.0).radius(3.0).center().border(1.5, if ext_on { l.accent } else { l.t.text_secondary.alpha(0.6) });
    let check = if ext_on { check.fill(l.accent).child(text("✓").font(TextStyle::new(10.0, 700.0)).color(Color::WHITE)) } else { check };
    let include_ext = row()
        .key("batch-ext")
        .role("checkbox")
        .label("Include extension")
        .checked(ext_on)
        .gap(6.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .help("Rename the extension too (⌥E); off, only the name before it changes")
        .on_press_tx(|s: &mut State, tx| {
            tx.instant();
            s.batch_set(|b| b.include_ext = !b.include_ext);
        })
        .child(check)
        .child(text("Include extension").font(SMALL).color(l.t.text));
    let number = row()
        .w_full()
        .gap(8.0)
        .center_y()
        .child(label("Number"))
        .child(text("Start").font(SMALL).color(l.t.text_secondary).a11y_hidden())
        .child(field(Field::Start, &b.start, "batch-start", "Number start", "1", Some(64.0)))
        .child(text("Step").font(SMALL).color(l.t.text_secondary).a11y_hidden())
        .child(field(Field::Step, &b.step, "batch-step", "Number step", "1", Some(52.0)))
        .child(el().w(10.0))
        .child(text("Case").font(SMALL).color(l.t.text_secondary).a11y_hidden())
        .child(cases)
        .child(el().grow(1.0))
        .child(include_ext);
    let fields = col().key("batch-fields").w_full().px(16.0).py(12.0).gap(8.0).child(find).child(replace).child(number);
    // The preview: only the rows in view are built.
    let count = b.items.len();
    let rows = col()
        .key("batch-rows")
        .role("list")
        .label("New names")
        .w_full()
        .growth(GrowthSpec { spring: rune::GROWTH, edge: rune::edge::NO_MASK })
        .virtual_rows(
            count,
            ROW_H,
            |_: &State, i| Cow::Owned(format!("b{i}")),
            move |s: &State, i| match &s.batch {
                Some(b) => batch_row(b, i, l),
                None => el(),
            },
        )
        // Keys are row numbers, the same whatever the rows hold.
        .virtual_rows_version(0u8);
    let head = row()
        .key("batch-head")
        .w_full()
        .h(24.0)
        .px(16.0)
        .gap(12.0)
        .center_y()
        .child(
            text(format!("{} item{}", crate::search::count(count as u64), if count == 1 { "" } else { "s" }))
                .font(SMALL)
                .color(l.t.text_secondary)
                .grow(1.0)
                .min_w(0.0),
        )
        .child(el().w(14.0))
        .child(text("New name").font(SMALL).color(l.t.text_secondary).grow(1.0).min_w(0.0));
    let h = (count as f32).min(BATCH_ROWS) * ROW_H;
    let list = col().key("batch-list").w_full().h(h).scroll(ScrollSpec { stick_bottom: false }).child(rows);
    // The summary, and Rename.
    let blocked = b.blocked().or_else(|| s.busy().then(|| s.busy_why().to_string()));
    let (summary, color) = match &b.error {
        Some(e) => (e.clone(), red()),
        None if b.plan.problems > 0 => (b.plan.summary(), red()),
        None if b.plan.changes > 0 => (b.plan.summary(), l.accent),
        None => (b.plan.summary(), l.t.text_secondary),
    };
    let summary = if b.stale() || !b.known() { format!("{summary} …") } else { summary };
    let button = |label: String, key: &'static str, yes: bool, on: bool| {
        let r = row()
            .key(key)
            .role("button")
            .disabled(!on)
            .h(28.0)
            .px(14.0)
            .center_y()
            .radius(6.0)
            .cursor(if on { Cursor::Pointer } else { Cursor::Default })
            .on_press_tx(move |s: &mut State, tx| {
                tx.instant();
                if yes { s.batch_apply(tx) } else { s.close_batch(tx) }
            })
            .child(text(label).font(TextStyle::new(13.0, 500.0)).color(if yes && on {
                Color::WHITE
            } else if on {
                l.t.text
            } else {
                l.t.text_secondary.alpha(0.6)
            }));
        match (yes, on) {
            (true, true) => r.fill(l.accent),
            (_, _) => r.fill(l.t.control_off).hover(l.t.hover),
        }
    };
    let key = |spec: &str| crate::commands::parse_chord(spec).map(|c| c.label()).unwrap_or_default();
    let rename_label = if b.plan.changes > 0 && blocked.is_none() {
        format!("Rename {}", crate::search::count(b.plan.changes as u64))
    } else {
        "Rename".into()
    };
    let foot = row()
        .key("batch-foot")
        .w_full()
        .px(16.0)
        .py(10.0)
        .gap(10.0)
        .center_y()
        .child(text(summary).key("batch-summary").role("status").font(NUM).color(color).truncate().grow(1.0).min_w(0.0))
        .child(
            text(match &blocked {
                Some(why) if b.error.is_none() && b.plan.problems == 0 && b.plan.changes > 0 => why.clone(),
                _ => format!("{} Rename   {} Cancel", key("enter"), key("escape")),
            })
            .font(SMALL)
            .color(l.t.text_secondary),
        )
        .child(button("Cancel".into(), "batch-cancel", false, true))
        .child(button(rename_label, "batch-apply", true, blocked.is_none()));
    let title = row()
        .w_full()
        .px(16.0)
        .pt(12.0)
        .child(text("Batch Rename").role("heading").level(1).font(TextStyle::new(14.0, 600.0)).color(l.t.text));
    let panel = overlay_card("batch", "Batch Rename", 820.0, l, |s: &mut State| {
        if let Some(b) = s.batch.take() {
            s.batch_rule = b.rule();
        }
    })
    .child(title)
    .child(fields)
    .child(hr(l))
    .child(head)
    .child(hr(l))
    .child(list)
    .child(hr(l))
    .child(foot);
    overlay_layer("batch-layer", panel)
}

/// One preview row: the old name, and the new one. Unchanged rows are dim; a problem shows in
/// red with its reason.
pub(super) fn batch_row(b: &crate::batch::BatchBox, i: usize, l: Look) -> El {
    use crate::rename::Status;
    let Some(item) = b.items.get(i) else { return el() };
    let new = b.plan.new.get(i).map_or(item.name.as_str(), String::as_str);
    let status = b.plan.status.get(i).cloned().unwrap_or(Status::Same);
    let dim = l.t.text_secondary.alpha(0.55);
    let (old_c, new_c) = match status {
        Status::Same | Status::Kept(_) => (dim, dim),
        Status::Change => (l.t.text_secondary, l.t.text),
        Status::Problem(_) => (l.t.text_secondary, red()),
    };
    let mut new_col = row()
        .grow(1.0)
        .min_w(0.0)
        .gap(8.0)
        .center_y()
        .child(text(new.to_string()).font(BODY).color(new_c).truncate_middle().shrink().min_w(0.0));
    match &status {
        Status::Problem(why) => new_col = new_col.child(text(why.clone()).font(SMALL).color(red()).truncate().grow(1.0).min_w(0.0)),
        Status::Kept(why) => new_col = new_col.child(text(why.to_string()).font(SMALL).color(dim).truncate().grow(1.0).min_w(0.0)),
        _ => {}
    }
    let r = row()
        .role("listitem")
        .w_full()
        .h(ROW_H)
        .px(16.0)
        .gap(12.0)
        .center_y()
        .help(fs::pretty(&b.paths[i]))
        .child(text(item.name.clone()).font(BODY).color(old_c).truncate_middle().grow(1.0).min_w(0.0))
        .child(text("→").label("becomes").font(BODY).color(dim).w(14.0))
        .child(new_col);
    if matches!(status, Status::Problem(_)) {
        r.fill(red().alpha(0.10))
    } else if i % 2 == 1 {
        r.fill(l.stripe)
    } else {
        r
    }
}
