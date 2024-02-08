use std::collections::{BTreeMap, BTreeSet};
use std::option::Option;
use std::path::Path;
use std::process::Command;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AqueryOutput {
    actions: Vec<Action>,
    targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Action {
    target_id: u32,
    file_contents: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Target {
    id: u32,
    label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrateSpec {
    pub crate_id: String,
    pub display_name: String,
    pub edition: String,
    pub root_module: String,
    pub is_workspace_member: bool,
    pub deps: BTreeSet<String>,
    pub proc_macro_dylib_path: Option<String>,
    pub source: Option<CrateSpecSource>,
    pub cfg: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub target: String,
    pub crate_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrateSpecSource {
    pub exclude_dirs: Vec<String>,
    pub include_dirs: Vec<String>,
}

pub fn get_crate_specs(
    bazel: &Path,
    workspace: &Path,
    additional_bazel_flags: &[String],
    targets: &[String],
    rules_rust_name: &str,
) -> anyhow::Result<BTreeSet<CrateSpec>> {
    log::debug!("Get crate specs with targets: {:?}", targets);
    let target_pattern = targets
        .iter()
        .map(|t| format!("deps({t})"))
        .collect::<Vec<_>>()
        .join("+");

    let aquery_output = Command::new(bazel)
        .current_dir(workspace)
        .arg("aquery")
        .arg("--noinclude_commandline")
        .arg("--noinclude_artifacts")
        .arg("--include_aspects")
        .arg("--include_file_write_contents")
        .arg(format!(
            "--aspects={rules_rust_name}//rust:defs.bzl%rust_analyzer_aspect"
        ))
        .arg("--output_groups=rust_analyzer_crate_spec")
        .arg(format!(
            r#"outputs(".*[.]rust_analyzer_crate_spec",{target_pattern})"#
        ))
        .arg("--output=jsonproto")
        .args(additional_bazel_flags)
        .output()?;

    consolidate_crate_specs(parse_aquery_output(&String::from_utf8(
        aquery_output.stdout,
    )?)?)
}

fn parse_aquery_output(aquery_stdout: &str) -> anyhow::Result<Vec<CrateSpec>> {
    let out: AqueryOutput = serde_json::from_str(aquery_stdout).map_err(|_| {
        // Parsing to `AqueryOutput` failed, try parsing into a `serde_json::Value`:
        match serde_json::from_str::<serde_json::Value>(aquery_stdout) {
            Ok(serde_json::Value::Object(_)) => {
                // If the JSON is an object, it's likely that the aquery command failed.
                anyhow::anyhow!("Aquery returned an empty result, are there any Rust targets in the specified paths?.")
            }
            _ => {
                anyhow::anyhow!("Failed to parse aquery output as JSON")
            }
        }
    })?;

    let targets = out
        .targets
        .iter()
        .map(|a| (a.id, a))
        .collect::<BTreeMap<_, _>>();

    let mut crate_specs = Vec::new();
    for action in out.actions {
        crate_specs.push(
            serde_json::from_str(&action.file_contents).with_context(|| {
                format!(
                    "Failed to deserialize crate_spec for target {}",
                    targets
                        .get(&action.target_id)
                        .expect("internal consistency error in bazel output")
                        .label,
                )
            })?,
        );
    }

    Ok(crate_specs)
}

/// Read all crate specs, deduplicating crates with the same ID. This happens when
/// a rust_test depends on a rust_library, for example.
fn consolidate_crate_specs(crate_specs: Vec<CrateSpec>) -> anyhow::Result<BTreeSet<CrateSpec>> {
    let mut consolidated_specs: BTreeMap<String, CrateSpec> = BTreeMap::new();
    for mut spec in crate_specs.into_iter() {
        log::debug!("{:?}", spec);
        if let Some(existing) = consolidated_specs.get_mut(&spec.crate_id) {
            existing.deps.extend(spec.deps);

            spec.cfg.retain(|cfg| !existing.cfg.contains(cfg));
            existing.cfg.extend(spec.cfg);

            // display_name should match the library's crate name because Rust Analyzer
            // seems to use display_name for matching crate entries in rust-project.json
            // against symbols in source files. For more details, see
            // https://github.com/bazelbuild/rules_rust/issues/1032
            if spec.crate_type == "rlib" {
                existing.display_name = spec.display_name;
                existing.crate_type = "rlib".into();
            }

            // For proc-macro crates that exist within the workspace, there will be a
            // generated crate-spec in both the fastbuild and opt-exec configuration.
            // Prefer proc macro paths with an opt-exec component in the path.
            if let Some(dylib_path) = spec.proc_macro_dylib_path.as_ref() {
                const OPT_PATH_COMPONENT: &str = "-opt-exec-";
                if dylib_path.contains(OPT_PATH_COMPONENT) {
                    existing.proc_macro_dylib_path.replace(dylib_path.clone());
                }
            }
        } else {
            consolidated_specs.insert(spec.crate_id.clone(), spec);
        }
    }

    Ok(consolidated_specs.into_values().collect())
}

#[cfg(test)]
mod test {
    use super::*;
    use itertools::Itertools;

    #[test]
    fn consolidate_lib_then_test_specs() {
        let crate_specs = vec![
            CrateSpec {
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::from(["ID-lib_dep.rs".into()]),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "rlib".into(),
            },
            CrateSpec {
                crate_id: "ID-extra_test_dep.rs".into(),
                display_name: "extra_test_dep".into(),
                edition: "2018".into(),
                root_module: "extra_test_dep.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "rlib".into(),
            },
            CrateSpec {
                crate_id: "ID-lib_dep.rs".into(),
                display_name: "lib_dep".into(),
                edition: "2018".into(),
                root_module: "lib_dep.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "rlib".into(),
            },
            CrateSpec {
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib_test".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::from(["ID-extra_test_dep.rs".into()]),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "bin".into(),
            },
        ];

        assert_eq!(
            consolidate_crate_specs(crate_specs).unwrap(),
            BTreeSet::from([
                CrateSpec {
                    crate_id: "ID-mylib.rs".into(),
                    display_name: "mylib".into(),
                    edition: "2018".into(),
                    root_module: "mylib.rs".into(),
                    is_workspace_member: true,
                    deps: BTreeSet::from(["ID-lib_dep.rs".into(), "ID-extra_test_dep.rs".into()]),
                    proc_macro_dylib_path: None,
                    source: None,
                    cfg: vec!["test".into(), "debug_assertions".into()],
                    env: BTreeMap::new(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    crate_type: "rlib".into(),
                },
                CrateSpec {
                    crate_id: "ID-extra_test_dep.rs".into(),
                    display_name: "extra_test_dep".into(),
                    edition: "2018".into(),
                    root_module: "extra_test_dep.rs".into(),
                    is_workspace_member: true,
                    deps: BTreeSet::new(),
                    proc_macro_dylib_path: None,
                    source: None,
                    cfg: vec!["test".into(), "debug_assertions".into()],
                    env: BTreeMap::new(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    crate_type: "rlib".into(),
                },
                CrateSpec {
                    crate_id: "ID-lib_dep.rs".into(),
                    display_name: "lib_dep".into(),
                    edition: "2018".into(),
                    root_module: "lib_dep.rs".into(),
                    is_workspace_member: true,
                    deps: BTreeSet::new(),
                    proc_macro_dylib_path: None,
                    source: None,
                    cfg: vec!["test".into(), "debug_assertions".into()],
                    env: BTreeMap::new(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    crate_type: "rlib".into(),
                },
            ])
        );
    }

    #[test]
    fn consolidate_test_then_lib_specs() {
        let crate_specs = vec![
            CrateSpec {
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib_test".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::from(["ID-extra_test_dep.rs".into()]),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "bin".into(),
            },
            CrateSpec {
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::from(["ID-lib_dep.rs".into()]),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "rlib".into(),
            },
            CrateSpec {
                crate_id: "ID-extra_test_dep.rs".into(),
                display_name: "extra_test_dep".into(),
                edition: "2018".into(),
                root_module: "extra_test_dep.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "rlib".into(),
            },
            CrateSpec {
                crate_id: "ID-lib_dep.rs".into(),
                display_name: "lib_dep".into(),
                edition: "2018".into(),
                root_module: "lib_dep.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "rlib".into(),
            },
        ];

        assert_eq!(
            consolidate_crate_specs(crate_specs).unwrap(),
            BTreeSet::from([
                CrateSpec {
                    crate_id: "ID-mylib.rs".into(),
                    display_name: "mylib".into(),
                    edition: "2018".into(),
                    root_module: "mylib.rs".into(),
                    is_workspace_member: true,
                    deps: BTreeSet::from(["ID-lib_dep.rs".into(), "ID-extra_test_dep.rs".into()]),
                    proc_macro_dylib_path: None,
                    source: None,
                    cfg: vec!["test".into(), "debug_assertions".into()],
                    env: BTreeMap::new(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    crate_type: "rlib".into(),
                },
                CrateSpec {
                    crate_id: "ID-extra_test_dep.rs".into(),
                    display_name: "extra_test_dep".into(),
                    edition: "2018".into(),
                    root_module: "extra_test_dep.rs".into(),
                    is_workspace_member: true,
                    deps: BTreeSet::new(),
                    proc_macro_dylib_path: None,
                    source: None,
                    cfg: vec!["test".into(), "debug_assertions".into()],
                    env: BTreeMap::new(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    crate_type: "rlib".into(),
                },
                CrateSpec {
                    crate_id: "ID-lib_dep.rs".into(),
                    display_name: "lib_dep".into(),
                    edition: "2018".into(),
                    root_module: "lib_dep.rs".into(),
                    is_workspace_member: true,
                    deps: BTreeSet::new(),
                    proc_macro_dylib_path: None,
                    source: None,
                    cfg: vec!["test".into(), "debug_assertions".into()],
                    env: BTreeMap::new(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    crate_type: "rlib".into(),
                },
            ])
        );
    }

    #[test]
    fn consolidate_lib_test_main_specs() {
        // mylib.rs is a library but has tests and an entry point, and mylib2.rs
        // depends on mylib.rs. The display_name of the library target mylib.rs
        // should be "mylib" no matter what order the crate specs is in.
        // Otherwise Rust Analyzer will not be able to resolve references to
        // mylib in mylib2.rs.
        let crate_specs = vec![
            CrateSpec {
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "rlib".into(),
            },
            CrateSpec {
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib_test".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "bin".into(),
            },
            CrateSpec {
                crate_id: "ID-mylib.rs".into(),
                display_name: "mylib_main".into(),
                edition: "2018".into(),
                root_module: "mylib.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "bin".into(),
            },
            CrateSpec {
                crate_id: "ID-mylib2.rs".into(),
                display_name: "mylib2".into(),
                edition: "2018".into(),
                root_module: "mylib2.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::from(["ID-mylib.rs".into()]),
                proc_macro_dylib_path: None,
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "rlib".into(),
            },
        ];

        for perm in crate_specs.into_iter().permutations(4) {
            assert_eq!(
                consolidate_crate_specs(perm).unwrap(),
                BTreeSet::from([
                    CrateSpec {
                        crate_id: "ID-mylib.rs".into(),
                        display_name: "mylib".into(),
                        edition: "2018".into(),
                        root_module: "mylib.rs".into(),
                        is_workspace_member: true,
                        deps: BTreeSet::from([]),
                        proc_macro_dylib_path: None,
                        source: None,
                        cfg: vec!["test".into(), "debug_assertions".into()],
                        env: BTreeMap::new(),
                        target: "x86_64-unknown-linux-gnu".into(),
                        crate_type: "rlib".into(),
                    },
                    CrateSpec {
                        crate_id: "ID-mylib2.rs".into(),
                        display_name: "mylib2".into(),
                        edition: "2018".into(),
                        root_module: "mylib2.rs".into(),
                        is_workspace_member: true,
                        deps: BTreeSet::from(["ID-mylib.rs".into()]),
                        proc_macro_dylib_path: None,
                        source: None,
                        cfg: vec!["test".into(), "debug_assertions".into()],
                        env: BTreeMap::new(),
                        target: "x86_64-unknown-linux-gnu".into(),
                        crate_type: "rlib".into(),
                    },
                ])
            );
        }
    }

    #[test]
    fn consolidate_proc_macro_prefer_exec() {
        // proc macro crates should prefer the -opt-exec- path which is always generated
        // during builds where it is used, while the fastbuild version would only be built
        // when explicitly building that target.
        let crate_specs = vec![
            CrateSpec {
                crate_id: "ID-myproc_macro.rs".into(),
                display_name: "myproc_macro".into(),
                edition: "2018".into(),
                root_module: "myproc_macro.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: Some(
                    "bazel-out/k8-opt-exec-F005BA11/bin/myproc_macro/libmyproc_macro-12345.so"
                        .into(),
                ),
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "proc_macro".into(),
            },
            CrateSpec {
                crate_id: "ID-myproc_macro.rs".into(),
                display_name: "myproc_macro".into(),
                edition: "2018".into(),
                root_module: "myproc_macro.rs".into(),
                is_workspace_member: true,
                deps: BTreeSet::new(),
                proc_macro_dylib_path: Some(
                    "bazel-out/k8-fastbuild/bin/myproc_macro/libmyproc_macro-12345.so".into(),
                ),
                source: None,
                cfg: vec!["test".into(), "debug_assertions".into()],
                env: BTreeMap::new(),
                target: "x86_64-unknown-linux-gnu".into(),
                crate_type: "proc_macro".into(),
            },
        ];

        for perm in crate_specs.into_iter().permutations(2) {
            assert_eq!(
                consolidate_crate_specs(perm).unwrap(),
                BTreeSet::from([CrateSpec {
                    crate_id: "ID-myproc_macro.rs".into(),
                    display_name: "myproc_macro".into(),
                    edition: "2018".into(),
                    root_module: "myproc_macro.rs".into(),
                    is_workspace_member: true,
                    deps: BTreeSet::new(),
                    proc_macro_dylib_path: Some(
                        "bazel-out/k8-opt-exec-F005BA11/bin/myproc_macro/libmyproc_macro-12345.so"
                            .into()
                    ),
                    source: None,
                    cfg: vec!["test".into(), "debug_assertions".into()],
                    env: BTreeMap::new(),
                    target: "x86_64-unknown-linux-gnu".into(),
                    crate_type: "proc_macro".into(),
                },])
            );
        }
    }
}
