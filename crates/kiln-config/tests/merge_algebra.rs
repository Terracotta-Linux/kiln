//! Property tests for the merge algebra.
//!
//! > `proptest`: union is commutative and associative; conflicts are detected
//! > regardless of include order; reordering a file's lines never changes
//! > `config_id`.
//!
//! These run the *whole* frontend against generated files on disk rather than
//! calling `merge` directly, because the properties are promises about
//! configurations, not about a function.

use kiln_config::Options;
use proptest::prelude::*;
use std::path::Path;

/// A generated leaf file: some scalars, some list members.
#[derive(Debug, Clone)]
struct Leaf {
    timeout: Option<i64>,
    kernel: Option<String>,
    packages: Vec<String>,
    cmdline: Vec<String>,
}

impl Leaf {
    fn render(&self, includes: &[String]) -> String {
        let mut s = String::from("kiln = 1\n");
        if !includes.is_empty() {
            let refs: Vec<String> = includes.iter().map(|i| format!("\"{i}\"")).collect();
            s.push_str(&format!("include = [{}]\n", refs.join(", ")));
        }
        if self.timeout.is_some() || self.kernel.is_some() {
            if let Some(t) = self.timeout {
                s.push_str(&format!("\n[boot]\ntimeout = {t}\n"));
            }
        }
        if self.kernel.is_some() || !self.cmdline.is_empty() {
            s.push_str("\n[kernel]\n");
            if let Some(k) = &self.kernel {
                s.push_str(&format!("package = \"{k}\"\n"));
            }
            if !self.cmdline.is_empty() {
                let q: Vec<String> = self.cmdline.iter().map(|c| format!("\"{c}\"")).collect();
                s.push_str(&format!("cmdline = [{}]\n", q.join(", ")));
            }
        }
        if !self.packages.is_empty() {
            let q: Vec<String> = self.packages.iter().map(|c| format!("\"{c}\"")).collect();
            s.push_str(&format!("\n[packages]\nrepo = [{}]\n", q.join(", ")));
        }
        s
    }
}

fn word() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["alpha", "beta", "gamma", "delta", "epsilon"]).prop_map(String::from)
}

fn leaf() -> impl Strategy<Value = Leaf> {
    (
        prop::option::of(0i64..10),
        prop::option::of(
            prop::sample::select(vec!["linux", "linux-lts", "linux-zen"]).prop_map(String::from),
        ),
        prop::collection::vec(word(), 0..4),
        prop::collection::vec(word(), 0..4),
    )
        .prop_map(|(timeout, kernel, packages, cmdline)| Leaf {
            timeout,
            kernel,
            packages,
            cmdline,
        })
}

struct Case {
    dir: tempfile::TempDir,
}

impl Case {
    /// Write `system.toml` including `leaves` in the given order.
    fn build(leaves: &[Leaf], order: &[usize]) -> Case {
        let dir = tempfile::tempdir().expect("tempdir");
        let names: Vec<String> = order.iter().map(|i| format!("m{i}.toml")).collect();
        for (i, leaf) in leaves.iter().enumerate() {
            std::fs::write(dir.path().join(format!("m{i}.toml")), leaf.render(&[])).unwrap();
        }
        std::fs::write(
            dir.path().join("system.toml"),
            Leaf {
                timeout: None,
                kernel: None,
                packages: vec![],
                cmdline: vec![],
            }
            .render(&names),
        )
        .unwrap();
        Case { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

fn load(case: &Case) -> Result<String, String> {
    let opts = Options {
        allow_external_sources: false,
        module_root: None,
    };
    match kiln_config::load(Some(case.path()), &opts) {
        Ok(fe) => Ok(fe.manifest.config_id().to_string()),
        Err(errs) => Err(kiln_diag::render_all(&errs)),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// Rule 1 is a set union, so sibling order cannot matter — not to the
    /// result, and not to whether it is an error.
    #[test]
    fn sibling_order_does_not_matter(leaves in prop::collection::vec(leaf(), 2..4)) {
        let n = leaves.len();
        let forward: Vec<usize> = (0..n).collect();
        let reverse: Vec<usize> = (0..n).rev().collect();

        let a = load(&Case::build(&leaves, &forward));
        let b = load(&Case::build(&leaves, &reverse));

        match (a, b) {
            (Ok(x), Ok(y)) => prop_assert_eq!(x, y, "include order changed config_id"),
            // Rule 3: a conflict must be a conflict either way round.
            (Err(_), Err(_)) => {}
            (x, y) => prop_assert!(false, "include order changed whether it is valid:\n{:?}\n{:?}", x, y),
        }
    }

    /// "Reordering lines in a TOML file must never change `config_id`."
    #[test]
    fn list_order_within_a_file_does_not_matter(mut leaf in leaf()) {
        leaf.timeout = Some(1);
        let a = load(&Case::build(std::slice::from_ref(&leaf), &[0]));
        leaf.packages.reverse();
        leaf.cmdline.reverse();
        let b = load(&Case::build(std::slice::from_ref(&leaf), &[0]));
        prop_assert_eq!(a.clone().ok(), b.clone().ok(), "{:?} vs {:?}", a, b);
    }

    /// Union is associative: grouping the same leaves differently in the include
    /// tree must not change the result.
    #[test]
    fn union_is_associative(leaves in prop::collection::vec(leaf(), 3..4)) {
        // Flat: system includes m0, m1, m2.
        let flat = load(&Case::build(&leaves, &(0..leaves.len()).collect::<Vec<_>>()));

        // Nested: system includes m0 and a group that includes m1 and m2.
        let dir = tempfile::tempdir().unwrap();
        for (i, l) in leaves.iter().enumerate() {
            std::fs::write(dir.path().join(format!("m{i}.toml")), l.render(&[])).unwrap();
        }
        let empty = Leaf { timeout: None, kernel: None, packages: vec![], cmdline: vec![] };
        std::fs::write(
            dir.path().join("group.toml"),
            empty.render(&["m1.toml".into(), "m2.toml".into()]),
        ).unwrap();
        std::fs::write(
            dir.path().join("system.toml"),
            empty.render(&["m0.toml".into(), "group.toml".into()]),
        ).unwrap();

        let opts = Options { allow_external_sources: false, module_root: None };
        let nested = kiln_config::load(Some(dir.path()), &opts)
            .map(|fe| fe.manifest.config_id().to_string())
            .map_err(|e| kiln_diag::render_all(&e));

        // Nesting changes *depth*, which is what decides an override, so the two
        // agree exactly when nothing disagreed in the first place.
        if flat.is_ok() && nested.is_ok() {
            prop_assert_eq!(flat.unwrap(), nested.unwrap(), "regrouping changed the union");
        }
    }
}
