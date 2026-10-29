//! What a three-way merge leaves in conflict, and AVP can settle without asking anyone.

/// Settles the conflicts of a diff3 merge (diffy's markers) where both sides only added to what the
/// ancestor had: whole lines at the same place, or text inserted into the same lines. The line's
/// addition, then the task's (once when they are the same). Agents building domains side by side
/// all add an import and a `register` call in the same spot of a shared file (25/10 bench: 5
/// conflicts on 6 landings, all of that kind), or their type to the same `import type { A, B }`
/// line (29/10: 1,066 conflict answers on one file, settled by a small model dropping a side).
/// Nothing lands unproven, so two additions that do not go together show as a red proof, not a
/// silent mistake. None as soon as one conflict removes or changes text the ancestor had.
pub fn settle_additions(with_markers: &str) -> Option<String> {
    let mut out = String::new();
    let mut lines = with_markers.split_inclusive('\n');
    while let Some(line) = lines.next() {
        if !line.starts_with("<<<<<<< ") {
            out.push_str(line);
            continue;
        }
        let (mut mine, mut base, mut theirs) = (String::new(), String::new(), String::new());
        let mut part = 0;
        loop {
            let l = lines.next()?;
            if l.starts_with("||||||| ") {
                part = 1;
            } else if l.trim_end() == "=======" {
                part = 2;
            } else if l.starts_with(">>>>>>> ") {
                break;
            } else {
                [&mut mine, &mut base, &mut theirs][part].push_str(l);
            }
        }
        out.push_str(&settle(&base, &mine, &theirs)?);
    }
    Some(out)
}

/// One conflict: line by line when neither side added or removed a line (each line may have its
/// insertion), else as one text (a list item added after the last one, its comma on the line
/// before). An ancestor's part that is empty is whole lines added on both sides.
fn settle(base: &str, mine: &str, theirs: &str) -> Option<String> {
    fn lines(s: &str) -> Vec<&str> {
        s.split_inclusive('\n').collect()
    }
    let (b, m, t) = (lines(base), lines(mine), lines(theirs));
    if !b.is_empty() && b.len() == m.len() && b.len() == t.len() {
        return b.iter().zip(&m).zip(&t).map(|((b, m), t)| both_inserted(b, m, t)).collect();
    }
    both_inserted(base, mine, theirs)
}

/// `base` with the insertion of each side, in the order of `base`; at the same place, the line's
/// first, once when both inserted the same text.
fn both_inserted(base: &str, mine: &str, theirs: &str) -> Option<String> {
    let (m, t) = (insertion(base, mine)?, insertion(base, theirs)?);
    let (first, second) = if m.0 < t.0 { (m, t) } else { (t, m) };
    let mut out = format!("{}{}", &base[..first.0], first.1);
    if first != second {
        out.push_str(&base[first.0..second.0]);
        out.push_str(second.1);
    }
    out.push_str(&base[second.0..]);
    Some(out)
}

/// Where `side` is `base` with one run of text inserted, compared character by character: the place
/// in `base` (after all they have in common at the start) and the text. None when `side` removed or
/// changed something of `base`.
fn insertion<'a>(base: &str, side: &'a str) -> Option<(usize, &'a str)> {
    let at: usize = base.chars().zip(side.chars()).take_while(|(b, s)| b == s).map(|(b, _)| b.len_utf8()).sum();
    let rest = &base[at..];
    let end = side.len().checked_sub(rest.len())?;
    (end >= at && side.ends_with(rest)).then(|| side.get(at..end)).flatten().map(|text| (at, text))
}

#[cfg(test)]
mod tests {
    use super::settle_additions;

    #[test]
    fn additions_at_the_same_place_are_both_kept_changes_are_not() {
        let base = "import a;\n\nfn app() {\n  a(app);\n}\n";
        let task = "import a;\nimport b;\n\nfn app() {\n  a(app);\n  b(app);\n}\n";
        let line = "import a;\nimport c;\n\nfn app() {\n  a(app);\n  c(app);\n}\n";
        let merged = diffy::merge(base, task, line).expect_err("a conflict for diffy");
        assert_eq!(settle_additions(&merged).as_deref(), Some("import a;\nimport c;\nimport b;\n\nfn app() {\n  a(app);\n  c(app);\n  b(app);\n}\n"));
        let changed = diffy::merge(base, "import a;\n\nfn app() {\n  b(app);\n}\n", "import a;\n\nfn app() {\n  c(app);\n}\n").expect_err("a conflict");
        assert_eq!(settle_additions(&changed), None);
    }

    #[test]
    fn insertions_into_the_same_import_line_are_both_kept() {
        let base = "import type { Customer, Invoice } from \"./types.js\";\n\nexport const store = new Store();\n";
        let task = "import type { Customer, Invoice, Payment } from \"./types.js\";\n\nexport const store = new Store();\n";
        let line = "import type { Customer, Invoice, Refund } from \"./types.js\";\n\nexport const store = new Store();\n";
        let merged = diffy::merge(base, task, line).expect_err("a conflict for diffy");
        assert_eq!(settle_additions(&merged).as_deref(), Some("import type { Customer, Invoice, Refund, Payment } from \"./types.js\";\n\nexport const store = new Store();\n"));
    }

    #[test]
    fn insertions_into_a_multi_line_object_literal_are_both_kept() {
        // Each side adds its collection after the last one (its comma on the line before), and to
        // both lists of the line under it.
        let base = "save({\n  customers,\n  invoices\n});\nconst all = [customers, invoices];\n";
        let task = "save({\n  customers,\n  invoices,\n  payments\n});\nconst all = [customers, invoices, payments];\n";
        let line = "save({\n  customers,\n  invoices,\n  refunds\n});\nconst all = [customers, invoices, refunds];\n";
        let merged = diffy::merge(base, task, line).expect_err("a conflict for diffy");
        assert_eq!(settle_additions(&merged).as_deref(), Some("save({\n  customers,\n  invoices,\n  refunds,\n  payments\n});\nconst all = [customers, invoices, refunds, payments];\n"));
        // A conflict over two lines, each with its insertion on both sides.
        let base = "const keys = [\"customers\", \"invoices\"];\nconst all = [customers, invoices];\n";
        let task = "const keys = [\"customers\", \"invoices\", \"payments\"];\nconst all = [customers, invoices, payments];\n";
        let line = "const keys = [\"customers\", \"invoices\", \"refunds\"];\nconst all = [customers, invoices, refunds];\n";
        let merged = diffy::merge(base, task, line).expect_err("a conflict for diffy");
        assert_eq!(settle_additions(&merged).as_deref(), Some("const keys = [\"customers\", \"invoices\", \"refunds\", \"payments\"];\nconst all = [customers, invoices, refunds, payments];\n"));
    }

    #[test]
    fn a_side_that_removes_or_rewrites_the_ancestors_text_stays_a_conflict() {
        let base = "import type { Customer, Invoice } from \"./types.js\";\n";
        let line = "import type { Customer, Invoice, Refund } from \"./types.js\";\n";
        // One type taken out while adding one, one renamed, one line dropped: nothing is settled.
        for task in ["import type { Customer, Payment } from \"./types.js\";\n", "import type { Client, Invoice, Payment } from \"./types.js\";\n", ""] {
            let merged = diffy::merge(base, task, line).expect_err("a conflict for diffy");
            assert_eq!(settle_additions(&merged), None, "{task:?}");
        }
    }
}
