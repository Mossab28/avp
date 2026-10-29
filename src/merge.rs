//! What a three-way merge leaves in conflict, and AVP can settle without asking anyone.

/// Settles the conflicts of a diff3 merge (diffy's markers) where both sides only added lines at
/// the same place, the ancestor's part empty: the line's lines, then the task's (once when they
/// are the same). Agents building domains side by side all add an import and a `register` call in
/// the same spot of a shared file, and every one of those was a conflict to settle by hand (25/10
/// bench: 5 conflicts on 6 landings, all of that kind). Nothing lands unproven, so two additions
/// that do not go together show as a red proof, not a silent mistake. None as soon as one conflict
/// changes lines the ancestor had.
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
        if !base.is_empty() {
            return None;
        }
        out.push_str(&theirs);
        if mine != theirs {
            out.push_str(&mine);
        }
    }
    Some(out)
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
}
