use super::*;
use std::fs;
use tempfile::TempDir;

struct Fixture(TempDir);
impl Fixture {
    fn new() -> Self {
        let f = Self(tempfile::tempdir().unwrap());
        for name in CRATES {
            fs::create_dir_all(f.0.path().join("crates").join(name)).unwrap();
        }
        f
    }
    fn put(&self, path: &str, text: &str) {
        let p = self.0.path().join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    fn source(text: &str) -> Self {
        let f = Self::new();
        f.put("crates/kio-core/src/lib.rs", text);
        f
    }
    fn report(&self) -> Value {
        measure(self.0.path(), ALLOWLIST).unwrap()
    }
    fn count(&self, category: &str) -> u64 {
        self.report()["categories"][category]["lines"]
            .as_u64()
            .unwrap()
    }
}
fn violations(report: &Value) -> &[Value] {
    report["os_condition_violations"].as_array().unwrap()
}

#[test]
fn comments_blank_lines_and_mixed_code() {
    let f = Fixture::source(
        "\n// c\n/// c\n//! c\n/* c */\n/** c */\n/*! c */\n/* outer\n /* nested */\n */\nfn a() {} // c\n/* c */ fn b() {} /* c */\n",
    );
    assert_eq!(f.count("common"), 2);
}
#[test]
fn strings_escape_raw_byte_c_and_multiline_are_code() {
    let f = Fixture::source(
        r######"let a = "// /* \" ";
let b = r"// /*";
let c = r###" " // /* "###;
let d = b"// /* \"";
let e = br##"// /*"##;
let f = c"// /* \"";
let g = "first

// middle
last";
let h = r#"first
/* middle */
last"#;
let ch = '/'; let q = '\''; let x = b'\x2f';
fn life<'a>(x: &'a str) -> &'a str { x }
"######,
    );
    assert_eq!(f.count("common"), 15);
}
#[test]
fn crlf_matches_lf() {
    let text = "// a\nlet a = \"first\n\nlast\";\n/* c */\n";
    assert_eq!(Fixture::source(text).count("common"), 3);
    assert_eq!(
        Fixture::source(&text.replace('\n', "\r\n")).count("common"),
        3
    );
}
#[test]
fn unterminated_comments_and_strings_fail() {
    for text in [
        "/*",
        "/* outer /* inner */",
        "let a = \"x",
        "let a = r###\"x\"##",
        "let a = b\"x",
        "let a = c\"x",
    ] {
        assert!(
            measure(Fixture::source(text).0.path(), ALLOWLIST).is_err(),
            "{text}"
        );
    }
}
#[test]
fn directory_and_filename_exclusions_are_exact() {
    let f = Fixture::new();
    for p in [
        "tests/a.rs",
        "benches/a.rs",
        "src/tests.rs",
        "src/unit_tests.rs",
        "src/test_support.rs",
    ] {
        f.put(
            &format!("crates/kio-core/{p}"),
            "#[cfg(windows)]\nfn ignored() {}\n",
        );
    }
    for p in [
        "build.rs",
        "examples/a.rs",
        "src/sub/tests/a.rs",
        "src/testing.rs",
    ] {
        f.put(&format!("crates/kio-core/{p}"), "fn included() {}\n");
    }
    assert_eq!(f.count("common"), 4);
    assert!(violations(&f.report()).is_empty());
}
#[test]
fn test_items_exclude_attributes_and_all_item_kinds() {
    let text = "#[allow(dead_code)]\n#[cfg(test)]\nmod tests {\n #[cfg(windows)] fn x() {}\n}\n#[cfg(all(test, windows))]\nfn f() {}\n#[cfg(test)]\nimpl T { fn f() {} }\n#[cfg(test)]\nuse foo::Bar;\nstruct S {\n #[cfg(test)]\n #[cfg(windows)]\n field: u8,\n other: u8,\n}\n#[cfg(test)]\nmod tests;\nfn production() {}\n";
    let f = Fixture::source(text);
    f.put(
        "crates/kio-core/src/tests.rs",
        "#[cfg(windows)] fn x() {}\n",
    );
    assert_eq!(f.count("common"), 4);
    assert!(violations(&f.report()).is_empty());
}
#[test]
fn file_inner_test_cfg_excludes_entire_source() {
    let f = Fixture::source("//! doc\n#![cfg(test)]\n#[cfg(windows)]\nfn x() {}\n");
    assert_eq!(f.count("common"), 0);
    assert!(!f.report()["has_violations"].as_bool().unwrap());
}
#[test]
fn any_and_not_test_are_production() {
    let f = Fixture::source(
        "#[cfg(any(test, windows))]\nfn a() {}\n#[cfg(not(test))]\n#[cfg(unix)]\nfn b() {}\n#[cfg(all(any(test, windows), unix))]\nfn c() {}\n",
    );
    assert_eq!(f.count("common"), 7);
    assert_eq!(violations(&f.report()).len(), 3);
}
#[test]
fn only_direct_test_in_outer_all_is_excluded() {
    let f = Fixture::source(
        "#[cfg(all(test, any(windows, unix)))]\nfn a() {}\n#[cfg(all(all(test), windows))]\nfn b() {}\n",
    );
    assert_eq!(f.count("common"), 2);
    assert_eq!(violations(&f.report()).len(), 1);
}
#[test]
fn module_filename_rule_both_directions_and_path_override() {
    for declaration in [
        "#[cfg(test)] mod wrong;",
        "mod tests;",
        "mod unit_tests;",
        "mod test_support;",
        "#[cfg(test)] #[path = \"wrong.rs\"] mod tests;",
        "#[path = \"tests.rs\"] mod production;",
    ] {
        assert!(
            measure(Fixture::source(declaration).0.path(), ALLOWLIST).is_err(),
            "{declaration}"
        );
    }
    for declaration in [
        "#[cfg(test)] mod tests;",
        "#[cfg(test)] mod unit_tests;",
        "#[cfg(test)] mod test_support;",
        "#[cfg(test)] #[path = \"tests.rs\"] mod arbitrary;",
        "#[path = \"ordinary.rs\"] mod tests;",
        "mod production;",
        "#[cfg(test)] mod arbitrary {}",
    ] {
        assert!(
            measure(Fixture::source(declaration).0.path(), ALLOWLIST).is_ok(),
            "{declaration}"
        );
    }
}
#[test]
fn os_conditions_each_syntax_and_identifier() {
    let text = "#[cfg(unix)] fn a() {}\n#![cfg(windows)]\n#[cfg_attr(target_os = \"linux\", allow(dead_code))] fn b() {}\nconst C: bool = cfg!(target_family = \"unix\");\n#[cfg(target_vendor = \"apple\")] fn c() {}\n#[cfg(target_env = \"gnu\")] fn d() {}\n";
    let f = Fixture::source(text);
    let r = f.report();
    assert_eq!(violations(&r).len(), 6);
    let kinds: Vec<_> = violations(&r)
        .iter()
        .map(|v| v["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["cfg", "cfg", "cfg_attr", "cfg!", "cfg", "cfg"]);
    assert_eq!(violations(&r)[2]["condition"], "target_os = \"linux\"");
    assert_eq!(violations(&r)[0]["line"], 1);
    assert_eq!(violations(&r)[0]["path"], "crates/kio-core/src/lib.rs");
}
#[test]
fn os_words_in_literals_comments_other_identifiers_and_cfg_attr_payload_do_not_match() {
    let f = Fixture::source(
        "// #[cfg(windows)]\nconst A: &str = \"cfg!(unix)\";\nconst B: &str = r#\"#[cfg(target_os = \"linux\")]\"#;\n#[cfg(target_arch = \"x86_64\")] fn a() {}\n#[cfg_attr(feature = \"unix\", cfg(windows))] fn b() {}\n#[cfg(my_windows)] fn c() {}\n",
    );
    assert!(violations(&f.report()).is_empty());
}
#[test]
fn platform_layout_four_layers_and_shared_dispatch() {
    let f = Fixture::new();
    f.put(
        "crates/kio-core/src/platform/mod.rs",
        "#[cfg(windows)] mod windows;\n#[cfg(unix)] pub use unix::*;\nfn common() {}\n",
    );
    for (name, count) in [("windows", 1), ("unix", 2), ("linux", 3), ("macos", 4)] {
        let path = if name == "linux" {
            format!("crates/kio-core/src/platform/{name}/child.rs")
        } else {
            format!("crates/kio-core/src/platform/{name}.rs")
        };
        f.put(
            &path,
            &"#[cfg(target_os = \"unsupported\")] fn x() {}\n".repeat(count),
        );
        assert_eq!(f.count(name), count as u64);
    }
    assert_eq!(f.count("common"), 3);
    assert!(violations(&f.report()).is_empty());
}
#[test]
fn platform_invalid_files_and_platform_rs_fail() {
    for p in [
        "src/platform.rs",
        "src/platform/other.rs",
        "src/platform/readme.md",
        "src/platform/mod/child.rs",
        "src/platform/other/child.rs",
    ] {
        let f = Fixture::new();
        f.put(&format!("crates/kio-core/{p}"), "fn x() {}\n");
        assert!(measure(f.0.path(), ALLOWLIST).is_err(), "{p}");
    }
}
#[test]
fn platform_mod_restricts_conditions_to_mod_and_use() {
    let f = Fixture::new();
    f.put("crates/kio-core/src/platform/mod.rs", "#[cfg(windows)] pub(crate) mod windows;\n#[cfg(unix)] pub use unix::{A, B};\n#[cfg(target_os = \"linux\")] fn f() {}\n#[cfg_attr(windows, derive(Debug))] struct S;\nconst B: bool = cfg!(unix);\n#![cfg(windows)]\n#[cfg(test)] #[cfg(windows)] fn test() {}\n");
    let r = f.report();
    assert_eq!(violations(&r).len(), 4);
    assert!(
        violations(&r)
            .iter()
            .all(|v| v["rule_id"] == "platform-dispatch")
    );
}
#[test]
fn tooling_ignores_layout_os_checks_and_file_limits() {
    let f = Fixture::new();
    f.put(
        "crates/kio-ci/src/platform.rs",
        &"#[cfg(windows)] fn x() {}\n".repeat(3001),
    );
    assert_eq!(f.count("tooling"), 3001);
    assert!(violations(&f.report()).is_empty());
    assert!(
        f.report()["oversized_product_files"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn category_and_file_boundaries_are_strict() {
    for (category, limit, location, tooling) in [
        ("common", 100000, "src/data", false),
        ("windows", 14000, "src/platform/windows", false),
        ("unix", 9000, "src/platform/unix", false),
        ("linux", 4000, "src/platform/linux", false),
        ("macos", 3000, "src/platform/macos", false),
        ("tooling", 65000, "src/data", true),
    ] {
        let f = Fixture::new();
        let name = if tooling { "kio-ci" } else { "kio-core" };
        let mut remaining = limit;
        let mut index = 0;
        while remaining > 0 {
            let n = remaining.min(3000);
            f.put(
                &format!("crates/{name}/{location}/{index}.rs"),
                &"fn x() {}\n".repeat(n),
            );
            remaining -= n;
            index += 1;
        }
        let r = f.report();
        assert_eq!(r["categories"][category]["limit"], limit);
        assert!(!r["categories"][category]["exceeded"].as_bool().unwrap());
        f.put(&format!("crates/{name}/{location}/extra.rs"), "fn x() {}\n");
        assert!(
            f.report()["categories"][category]["exceeded"]
                .as_bool()
                .unwrap()
        );
    }
    let f = Fixture::source(&"fn x() {}\n".repeat(3000));
    assert!(
        f.report()["oversized_product_files"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    f.put("crates/kio-core/src/lib.rs", &"fn x() {}\n".repeat(3001));
    assert_eq!(f.report()["oversized_product_files"][0]["lines"], 3001);
}
#[test]
fn allowlist_valid_and_invalid_registrations() {
    let f = Fixture::source("#[cfg(windows)] fn x() {}\n#[cfg(unix)] fn y() {}\n");
    let a = [Allowance {
        path: "crates/kio-core/src/lib.rs",
        rule_id: "common-os-condition",
        reason: "fixture exception",
    }];
    let r = measure(f.0.path(), &a).unwrap();
    assert!(violations(&r).is_empty());
    assert_eq!(r["allowlist_applications"][0]["count"], 2);
    assert!(!r["has_violations"].as_bool().unwrap());
    for (path, rule, reason) in [
        ("crates/kio-core/src/lib.rs", "common-os-condition", ""),
        ("crates/kio-core/src/lib.rs", "common-os-condition", "  "),
        ("crates/kio-core/src/lib.rs", "platform-dispatch", "unused"),
        (
            "crates/kio-core/src/missing.rs",
            "common-os-condition",
            "missing",
        ),
        ("crates/kio-core/src/lib.rs", "file-limit", "unsupported"),
    ] {
        assert!(
            measure(
                f.0.path(),
                &[Allowance {
                    path,
                    rule_id: rule,
                    reason
                }]
            )
            .is_err()
        );
    }
}
#[test]
fn unknown_crates_and_non_utf8_source_fail() {
    let f = Fixture::new();
    fs::create_dir(f.0.path().join("crates/kio-new")).unwrap();
    assert!(measure(f.0.path(), ALLOWLIST).is_err());
    let f = Fixture::source("fn x() {}\n");
    fs::write(f.0.path().join("crates/kio-core/src/lib.rs"), [0xff]).unwrap();
    assert!(measure(f.0.path(), ALLOWLIST).is_err());
}
#[cfg(unix)]
#[test]
fn file_directory_and_excluded_symlinks_fail() {
    use std::os::unix::fs::symlink;
    for path in ["src/link.rs", "src/link", "tests/link.rs", "src/tests.rs"] {
        let f = Fixture::new();
        let p = f.0.path().join("crates/kio-core").join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        symlink("missing", p).unwrap();
        assert!(measure(f.0.path(), ALLOWLIST).is_err(), "{path}");
    }
}
#[test]
fn cli_statuses_root_and_invalid_arguments() {
    let f = Fixture::source("fn x() {}\n");
    let args = || vec![OsString::from("--root"), f.0.path().as_os_str().to_owned()];
    assert_eq!(entry(args()).unwrap().1, 0);
    f.put("crates/kio-core/src/lib.rs", "#[cfg(windows)] fn x() {}\n");
    assert_eq!(entry(args()).unwrap().1, 1);
    f.put("crates/kio-core/src/lib.rs", "/* unclosed");
    assert!(entry(args()).is_err());
    for args in [
        vec!["--root"],
        vec!["--unknown"],
        vec!["--root", ".", "extra"],
        vec!["."],
    ] {
        assert!(entry(args.into_iter().map(OsString::from).collect()).is_err());
    }
}
#[test]
fn output_is_deterministic_and_sorted() {
    let f = Fixture::new();
    f.put("crates/kio-core/src/z.rs", "#[cfg(windows)] fn z() {}\n");
    f.put(
        "crates/kio-app/src/a.rs",
        "#[cfg(unix)] fn a() {}\n#[cfg(windows)] fn b() {}\n",
    );
    let r = f.report();
    assert_eq!(r.to_string(), f.report().to_string());
    assert_eq!(r["schema_id"], "kio-code-limits/v1");
    assert_eq!(violations(&r)[0]["path"], "crates/kio-app/src/a.rs");
    assert_eq!(violations(&r)[1]["line"], 2);
    assert_eq!(r["crates"]["kio-app"]["common"], 2);
}

#[test]
fn stacked_reverse_attributes_and_inherited_module_naming() {
    let f = Fixture::source(
        "#[cfg(windows)]\n#[cfg(test)]\n#[allow(dead_code)]\nmod tests {\n #[cfg(unix)] fn child() {}\n mod unit_tests;\n}\nfn production() {}\n",
    );
    assert_eq!(f.count("common"), 1);
    assert!(violations(&f.report()).is_empty());
    let f = Fixture::source("#[cfg(test)] mod tests { mod wrong; }\n");
    assert!(measure(f.0.path(), ALLOWLIST).is_err());
}
#[test]
fn generic_fields_signatures_use_const_static_and_const_fn_items() {
    let text = "struct S {\n #[cfg(test)]\n field: Result<A, B>,\n #[cfg(test)]\n callback: fn(A, B),\n live: u8,\n}\n#[cfg(test)]\nfn f<const N: usize>() -> Result<[u8; { N }], B>\nwhere A: Trait<{ N }>\n{ cfg!(windows); }\n#[cfg(test)]\nuse foo::{A, B}\n;\n#[cfg(test)]\nconst C: Foo = Foo { field: 1 }\n;\n#[cfg(test)]\nstatic S: Foo = Foo { field: 1 }\n;\n#[cfg(test)]\nconst fn f() -> bool { true }\n#[cfg(test)]\nconst LESS: bool = 1 < 2;\nfn production() {}\n";
    assert_eq!(Fixture::source(text).count("common"), 4);
}
#[test]
fn unicode_char_escapes_lifetimes_and_labels() {
    let f = Fixture::source(
        "let a = '🦀'; let b = '\\u{1f980}'; let c = '\\x2f';\nfn f<'a>(x: &'a str) { 'label: loop { break 'label; } }\n",
    );
    assert_eq!(f.count("common"), 2);
}
#[test]
fn multiline_condition_preserves_expression_and_start_line() {
    let f = Fixture::source("#[cfg(all(\n windows,\n target_arch = \"x86_64\",\n))]\nfn a() {}\n");
    let r = f.report();
    assert_eq!(violations(&r)[0]["line"], 1);
    assert_eq!(
        violations(&r)[0]["condition"],
        "all( windows, target_arch = \"x86_64\", )"
    );
}
#[test]
fn every_os_supports_file_and_directory_layout() {
    let f = Fixture::new();
    for os in ["windows", "unix", "linux", "macos"] {
        f.put(
            &format!("crates/kio-core/src/platform/{os}.rs"),
            "fn a() {}\n",
        );
        f.put(
            &format!("crates/kio-core/src/platform/{os}/mod.rs"),
            "fn b() {}\n",
        );
        f.put(
            &format!("crates/kio-core/src/platform/{os}/nested/child.rs"),
            "fn c() {}\n",
        );
        assert_eq!(f.count(os), 3);
    }
}
#[test]
fn path_module_literals_raw_and_escaped() {
    for declaration in [
        r##"#[cfg(test)] #[path = r#"nested/unit_tests.rs"#] mod arbitrary;"##,
        r#"#[cfg(test)] #[path = "nested/\u{74}ests.rs"] mod arbitrary;"#,
    ] {
        assert!(
            measure(Fixture::source(declaration).0.path(), ALLOWLIST).is_ok(),
            "{declaration}"
        );
    }
}
#[test]
fn production_code_on_test_item_line_remains_counted() {
    let f = Fixture::source(
        "fn a() {} #[cfg(test)] fn test() {} fn b() {}\n#[cfg(test)] fn test2() {}\n",
    );
    assert_eq!(f.count("common"), 1);
}
#[test]
fn excluded_sources_still_require_utf8_and_crates_directory_is_not_linked() {
    let f = Fixture::new();
    f.put("crates/kio-core/src/tests.rs", "");
    fs::write(f.0.path().join("crates/kio-core/src/tests.rs"), [0xff]).unwrap();
    assert!(measure(f.0.path(), ALLOWLIST).is_err());
    #[cfg(unix)]
    {
        let f = Fixture::new();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(f.0.path().join("crates"), outside.path().join("crates"))
            .unwrap();
        assert!(measure(outside.path(), ALLOWLIST).is_err());
    }
}

#[test]
fn path_override_requires_exact_test_source_filename() {
    for path in [
        "tests",
        "unit_tests",
        "test_support",
        "tests.txt",
        "unit_tests.txt",
    ] {
        let declaration = format!("#[cfg(test)] #[path = \"{path}\"] mod arbitrary;");
        assert!(
            measure(Fixture::source(&declaration).0.path(), ALLOWLIST).is_err(),
            "{path}"
        );
    }
    for path in ["tests.rs", "unit_tests.rs", "test_support.rs"] {
        let declaration = format!("#[path = \"{path}\"] mod arbitrary;");
        assert!(
            measure(Fixture::source(&declaration).0.path(), ALLOWLIST).is_err(),
            "{path}"
        );
    }
    assert!(
        measure(
            Fixture::source("#[path = \"tests\"] mod arbitrary;")
                .0
                .path(),
            ALLOWLIST
        )
        .is_ok()
    );
}

#[test]
fn excluded_sources_still_require_closed_comments_and_strings() {
    for path in [
        "tests/test.rs",
        "benches/bench.rs",
        "src/tests.rs",
        "src/unit_tests.rs",
        "src/test_support.rs",
    ] {
        for text in ["/* unclosed", "let a = \"unclosed", "let a = r#\"unclosed"] {
            let f = Fixture::new();
            f.put(&format!("crates/kio-core/{path}"), text);
            assert!(measure(f.0.path(), ALLOWLIST).is_err(), "{path}: {text}");
        }
    }
}

#[test]
fn test_function_where_clause_commas_do_not_terminate_item() {
    let f = Fixture::source(
        "#[cfg(test)]\nfn f<A, B>()\nwhere\n A: Send,\n B: Send,\n{\n #[cfg(windows)] let x = 1;\n}\nfn product() {}\n",
    );
    assert_eq!(f.count("common"), 1);
    assert!(violations(&f.report()).is_empty());
}
#[test]
fn test_enum_discriminant_shift_keeps_next_production_variant() {
    let f =
        Fixture::source("enum E {\n #[cfg(test)]\n A = 1 << 2,\n #[cfg(windows)]\n B = 5,\n}\n");
    let r = f.report();
    assert_eq!(f.count("common"), 4);
    assert_eq!(violations(&r).len(), 1);
    assert_eq!(violations(&r)[0]["line"], 4);
}
#[test]
fn test_braced_variant_includes_trailing_comma() {
    let f = Fixture::source("enum E {\n #[cfg(test)]\n A { value: u8 }\n ,\n B,\n}\n");
    assert_eq!(f.count("common"), 3);
}

#[test]
fn test_discriminants_distinguish_operators_and_expression_generics() {
    for expression in [
        "1 << 2",
        "8 >> 1",
        "foo::<A, B>()",
        "<T as Trait<A, B>>::N",
        "<Pair<A, B>>::N",
    ] {
        let f = Fixture::source(&format!(
            "enum E {{\n #[cfg(test)]\n A = {expression},\n #[cfg(windows)]\n B = 5,\n}}\n"
        ));
        let r = f.report();
        assert_eq!(f.count("common"), 4, "{expression}");
        assert_eq!(violations(&r).len(), 1, "{expression}");
    }
}

#[test]
fn test_const_extern_function_does_not_consume_following_items() {
    let f = Fixture::source(
        "#[cfg(test)]\nconst extern \"C\" fn test() -> bool { true }\n#[cfg(windows)]\nfn product() {}\nconst C: u8 = 1;\n",
    );
    let r = f.report();
    assert_eq!(f.count("common"), 3);
    assert_eq!(violations(&r).len(), 1);
    assert_eq!(violations(&r)[0]["line"], 3);
}
#[test]
fn unicode_identifier_combining_mark_is_not_an_os_identifier() {
    let f = Fixture::source("#[cfg(windows\u{301})]\nfn custom() {}\n");
    assert_eq!(f.count("common"), 2);
    assert!(violations(&f.report()).is_empty());
}
#[test]
fn raw_identifiers_match_cfg_test_and_module_names() {
    let f = Fixture::source(
        "#[cfg(r#unix)]\nfn a() {}\n#[cfg(r#test)]\nmod r#tests;\n#[cfg(all(r#test, windows))]\nfn test() {}\n",
    );
    let r = f.report();
    assert_eq!(f.count("common"), 2);
    assert_eq!(violations(&r).len(), 1);
    assert_eq!(violations(&r)[0]["condition"], "r#unix");
}

#[test]
fn test_raw_abi_const_extern_function_does_not_consume_following_items() {
    for abi in [r#"r"C""#, r##"r#"C"#"##] {
        let f = Fixture::source(&format!(
            "#[cfg(test)]\nconst extern {abi} fn test() -> bool {{ true }}\n#[cfg(windows)]\nfn product() {{}}\nconst C: u8 = 1;\n"
        ));
        let r = f.report();
        assert_eq!(f.count("common"), 3, "{abi}");
        assert_eq!(violations(&r).len(), 1, "{abi}");
        assert_eq!(violations(&r)[0]["line"], 3);
    }
}
#[test]
fn unicode_whitespace_only_lines_are_blank() {
    let f = Fixture::source("\u{85}\n\u{2028}\n\u{200e}\n\u{200f}\nfn product() {}\n");
    assert_eq!(f.count("common"), 1);
}

#[test]
fn raw_attribute_and_macro_identifiers_use_canonical_names() {
    let f = Fixture::source(
        "#[r#cfg(r#unix)] fn a() {}\n#[r#cfg_attr(r#windows, allow(dead_code))] fn b() {}\nconst C: bool = r#cfg!(unix);\n#[r#cfg(test)] #[r#path = \"tests.rs\"] mod arbitrary;\n",
    );
    let r = f.report();
    assert_eq!(f.count("common"), 3);
    let kinds: Vec<_> = violations(&r)
        .iter()
        .map(|v| v["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["cfg", "cfg_attr", "cfg!"]);
}

#[test]
fn b2_inner_test_cfg_belongs_to_enclosing_module() {
    let f = Fixture::source(
        "#[cfg(windows)]\npub fn production() {}\n#[cfg(unix)]\nmod tests {\n #![cfg(test)]\n mod unit_tests;\n fn test() {}\n}\n",
    );
    assert_eq!(f.count("common"), 2);
    assert_eq!(violations(&f.report()).len(), 1);
    let text = format!(
        "{}mod tests {{ #![cfg(test)] fn test() {{}} }}\n",
        "fn production() {}\n".repeat(3001)
    );
    assert_eq!(
        Fixture::source(&text).report()["oversized_product_files"][0]["lines"],
        3001
    );
    let f = Fixture::source("fn production() {}\n#![cfg(test)]\n#[cfg(windows)] fn later() {}\n");
    assert!(measure(f.0.path(), ALLOWLIST).is_err());
    let f = Fixture::source("#![allow(dead_code)]\n#![cfg(test)]\nmod unit_tests;\n");
    assert_eq!(f.count("common"), 0);
    assert!(
        measure(
            Fixture::source("mod tests { #![cfg(test)] mod helper; }")
                .0
                .path(),
            ALLOWLIST
        )
        .is_err()
    );
}

#[test]
fn b2_test_item_stops_at_parent_delimiters() {
    let f = Fixture::source(
        "pub fn production(\n #[cfg(test)]\n arg: u8\n) {\n #[cfg(windows)]\n let _ = 1;\n}\n",
    );
    assert_eq!(f.count("common"), 5);
    assert_eq!(violations(&f.report()).len(), 1);
    assert_eq!(
        Fixture::source("struct S(\n #[cfg(test)]\n u8\n);\n#[cfg(windows)] fn product() {}\n")
            .count("common"),
        3
    );
    assert_eq!(
        Fixture::source(
            "struct S {\n #[cfg(test)]\n field: u8\n}\n#[cfg(windows)] fn product() {}\n"
        )
        .count("common"),
        3
    );
    assert_eq!(
        Fixture::source("let values = [\n #[cfg(test)]\n 1\n];\n#[cfg(windows)] fn product() {}\n")
            .count("common"),
        3
    );
    let text = format!(
        "fn production(\n #[cfg(test)] arg: u8\n) {{\n{}}}\n",
        "let _ = 1;\n".repeat(3001)
    );
    assert!(
        !Fixture::source(&text).report()["oversized_product_files"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn b2_cfg_macro_all_three_delimiters() {
    let f = Fixture::source(
        "const A: bool = cfg![windows];\nconst B: bool = cfg!{unix};\nconst C: bool = cfg!(target_os = \"linux\");\nconst D: bool = cfg![target_arch = \"x86_64\"];\n",
    );
    assert_eq!(violations(&f.report()).len(), 3);
    let f = Fixture::source("#[cfg(test)] fn test() { cfg![windows]; cfg!{unix}; }\n");
    assert!(violations(&f.report()).is_empty());
}

#[test]
fn b2_cfg_attr_checks_all_module_path_candidates() {
    for declaration in [
        "#[cfg_attr(not(test), path = \"tests.rs\")] mod production;",
        "#[cfg_attr(feature = \"x\", cfg_attr(feature = \"y\", path = \"unit_tests.rs\"))] mod production;",
        "#[cfg(test)] #[cfg_attr(feature = \"x\", path = \"helper.rs\")] mod tests;",
        "#[cfg(test)] #[cfg_attr(feature = \"x\", path = \"tests.rs\")] mod helper;",
        "#[cfg_attr(feature = \"x\", path = \"ordinary.rs\")] mod tests;",
        "#[path = \"ordinary.rs\"] #[cfg_attr(feature = \"x\", path = \"tests.rs\")] mod production;",
        "#[cfg(test)] #[cfg_attr(feature = \"x\", path = \"tests.rs\", path = \"ordinary.rs\")] mod tests;",
    ] {
        assert!(
            measure(Fixture::source(declaration).0.path(), ALLOWLIST).is_err(),
            "{declaration}"
        );
    }
    for declaration in [
        "#[cfg(test)] #[cfg_attr(feature = \"x\", path = \"unit_tests.rs\")] mod tests;",
        "#[path = \"ordinary.rs\"] #[cfg_attr(feature = \"x\", path = \"another.rs\")] mod tests;",
        "#[cfg(test)] #[path = \"tests.rs\"] #[cfg_attr(feature = \"x\", cfg_attr(feature = \"y\", path = \"unit_tests.rs\"))] mod helper;",
        "#[cfg_attr(feature = \"x\", allow(path = \"tests.rs\"))] mod production;",
    ] {
        assert!(
            measure(Fixture::source(declaration).0.path(), ALLOWLIST).is_ok(),
            "{declaration}"
        );
    }
}

#[test]
fn b2_test_files_are_excluded_before_platform_layout() {
    for path in [
        "src/platform/tests.rs",
        "src/platform/unit_tests.rs",
        "src/platform/test_support.rs",
        "src/platform/other/tests.rs",
        "src/platform/windows.rs/tests.rs",
    ] {
        let f = Fixture::new();
        f.put(
            &format!("crates/kio-core/{path}"),
            "#[cfg(windows)] fn test() {}\n",
        );
        assert_eq!(f.count("common"), 0, "{path}");
        assert!(violations(&f.report()).is_empty());
    }
    let f = Fixture::new();
    f.put(
        "crates/kio-core/src/platform/other/helper.rs",
        "#![cfg(test)]\nfn test() {}\n",
    );
    assert_eq!(f.count("common"), 0);
    f.put(
        "crates/kio-core/src/platform/other/production.rs",
        "fn production() {}\n",
    );
    assert!(measure(f.0.path(), ALLOWLIST).is_err());
    for contents in ["/* unclosed", "let x = \"unclosed"] {
        let f = Fixture::new();
        f.put("crates/kio-core/src/platform/tests.rs", contents);
        assert!(measure(f.0.path(), ALLOWLIST).is_err());
    }
    let f = Fixture::new();
    f.put("crates/kio-core/src/platform/tests.rs", "");
    fs::write(
        f.0.path().join("crates/kio-core/src/platform/tests.rs"),
        [0xff],
    )
    .unwrap();
    assert!(measure(f.0.path(), ALLOWLIST).is_err());
}

#[test]
fn b2_named_test_file_validates_nested_module_names() {
    for filename in ["tests.rs", "unit_tests.rs", "test_support.rs"] {
        let f = Fixture::new();
        f.put(&format!("crates/kio-core/src/{filename}"), "mod helper;\n");
        assert!(measure(f.0.path(), ALLOWLIST).is_err(), "{filename}");
        f.put(
            &format!("crates/kio-core/src/{filename}"),
            "mod unit_tests;\n#[path = \"tests.rs\"] mod helper;\n",
        );
        assert_eq!(f.count("common"), 0);
    }
    let f = Fixture::new();
    f.put("crates/kio-core/tests/integration.rs", "mod common;\n");
    f.put("crates/kio-core/benches/tests.rs", "mod common;\n");
    assert_eq!(f.count("common"), 0);
}

#[test]
fn b2_macro_tokens_are_not_module_declarations_or_test_scopes() {
    for text in [
        "macro_rules! discard { (mod tests;) => {}; }",
        "discard!(mod tests;);",
        "discard![mod tests;];",
        "discard!{mod tests;}",
        "macro_rules! discard { ($x:tt) => { mod tests; }; }",
        "discard!(#[cfg(unix)] mod tests;);",
    ] {
        assert!(
            measure(Fixture::source(text).0.path(), ALLOWLIST).is_err(),
            "{text}"
        );
    }
    for text in [
        "macro_rules! discard { (#![cfg(test)]) => {}; }",
        "discard!{mod tests { #![cfg(test)] }}",
    ] {
        let f = Fixture::source(&format!("{text}\n#[cfg(windows)] fn product() {{}}\n"));
        assert_eq!(f.count("common"), 2, "{text}");
        assert_eq!(violations(&f.report()).len(), 1, "{text}");
    }
}

#[test]
fn b2_bom_does_not_count_comment_at_file_limit() {
    let f = Fixture::source(&format!(
        "\u{feff}// comment only\n{}",
        "fn production() {}\n".repeat(3000)
    ));
    assert_eq!(f.count("common"), 3000);
    assert!(
        f.report()["oversized_product_files"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        Fixture::source("\u{feff}// comment only\nfn product() {}\n").count("common"),
        1
    );
    assert_eq!(
        Fixture::source("\u{feff}fn product() {}\n").count("common"),
        1
    );
}

#[test]
fn b2_module_path_filename_uses_both_separator_styles() {
    for path in [
        r"nested\tests.rs",
        r"nested/inner\unit_tests.rs",
        r"nested\inner/test_support.rs",
    ] {
        let declaration = format!("#[cfg(test)] #[path = r\"{path}\"] mod helper;");
        assert!(
            measure(Fixture::source(&declaration).0.path(), ALLOWLIST).is_ok(),
            "{path}"
        );
        let declaration = format!("#[path = r\"{path}\"] mod helper;");
        assert!(
            measure(Fixture::source(&declaration).0.path(), ALLOWLIST).is_err(),
            "{path}"
        );
    }
    let f = Fixture::source(
        r#"#[cfg(test)] #[cfg_attr(feature = "x", path = r"nested\ordinary.rs")] mod tests;"#,
    );
    assert!(measure(f.0.path(), ALLOWLIST).is_err());
}

#[test]
fn b2_condition_json_normalizes_whitespace_and_line_endings() {
    let text = "#[cfg(all(\n\t windows,\n target_arch = \"x86_64\",\n))]\nfn a() {}\nconst X: bool = cfg!{ all(\n unix,\n feature = \"x\" ) };\n";
    let lf = Fixture::source(text).report();
    let crlf = Fixture::source(&text.replace('\n', "\r\n")).report();
    assert_eq!(lf, crlf);
    assert_eq!(
        violations(&lf)[0]["condition"],
        "all( windows, target_arch = \"x86_64\", )"
    );
    assert_eq!(violations(&lf)[0]["line"], 1);
    assert_eq!(violations(&lf).len(), 2);
}

#[test]
fn b2_macro_tokens_do_not_authorize_platform_dispatch() {
    for source in [
        "discard!(#[cfg(windows)] mod production {});",
        "discard![#[cfg(windows)] mod production {}];",
        "discard!{#[cfg(windows)] mod production {}}",
        "discard!(#[cfg(unix)] use production::Item);",
        "discard![#[cfg(unix)] use production::Item];",
        "discard!{#[cfg(unix)] use production::Item}",
        "macro_rules! discard { () => { #[cfg(windows)] mod production {} }; }",
        "macro_rules! discard { () => { #[cfg(unix)] use production::Item; }; }",
        "discard!(cfg!(windows));",
    ] {
        let f = Fixture::new();
        f.put("crates/kio-core/src/platform/mod.rs", source);
        let r = f.report();
        assert_eq!(violations(&r).len(), 1, "{source}");
        assert_eq!(violations(&r)[0]["rule_id"], "platform-dispatch");
        assert!(r["has_violations"].as_bool().unwrap());
    }
    let f = Fixture::new();
    f.put(
        "crates/kio-core/src/platform/mod.rs",
        "#[cfg(windows)] mod production;\n#[cfg(unix)] use production::Item;\n",
    );
    assert!(violations(&f.report()).is_empty());
}

#[test]
fn b3_macro_out_of_line_modules_fail_conservatively() {
    for source in [
        "macro_rules! declare { () => { mod tests; }; }",
        "declare!(mod ordinary;);",
        "declare![#[path = \"ordinary.rs\"] mod tests;];",
        "declare!{mod unit_tests;}",
        "macro_rules! declare { ($name:ident) => { mod $name; }; }",
        "macro_rules! declare { ($name:ident) => { #[path = \"tests.rs\"] mod $name; }; }",
        "macro_rules! declare { ($semi:tt) => { mod tests $semi }; }",
    ] {
        assert!(
            measure(Fixture::source(source).0.path(), ALLOWLIST).is_err(),
            "{source}"
        );
    }
    for source in [
        "declare!{mod ordinary { fn f() {} }}",
        "macro_rules! declare { ($name:ident) => { mod $name { fn f() {} } }; }",
    ] {
        assert!(
            measure(Fixture::source(source).0.path(), ALLOWLIST).is_ok(),
            "{source}"
        );
    }
}

#[test]
fn b3_last_test_generic_stops_at_closing_angle() {
    for parameter in ["T", "const N: usize", "'a"] {
        let f = Fixture::source(&format!(
            "pub fn product<\n #[cfg(test)]\n {parameter}\n>() {{\n #[cfg(unix)]\n let _ = 1;\n}}\n"
        ));
        assert_eq!(f.count("common"), 5, "{parameter}");
        assert_eq!(violations(&f.report()).len(), 1);
    }
    for parameter in [
        "T: Into<Vec<Vec<u8>>>",
        "T: Fn() -> usize",
        "T = Wrapper<{ 2 > 1 }>",
        "const N: bool = { 2 > 1 }",
    ] {
        let f = Fixture::source(&format!(
            "struct Product<\n#[cfg(test)]\n{parameter}\n>;\n#[cfg(windows)] fn production() {{}}\n"
        ));
        assert_eq!(f.count("common"), 3, "{parameter}");
        assert_eq!(violations(&f.report()).len(), 1);
    }
    for (source, count) in [
        (
            "pub struct S<\n#[cfg(test)]\nT\n> {\n#[cfg(test)]\nmarker: std::marker::PhantomData<T>,\n#[cfg(unix)]\npub x: u8,\n}\n",
            5,
        ),
        (
            "pub struct S<\n#[cfg(test)]\nconst N: usize\n> {\n#[cfg(unix)]\npub x: u8,\n}\npub const C: u8 = 1;\n",
            6,
        ),
    ] {
        let f = Fixture::source(source);
        assert_eq!(f.count("common"), count);
        assert_eq!(violations(&f.report()).len(), 1);
    }
    let f = Fixture::source(&format!(
        "pub fn product<\n#[cfg(test)]\nT\n>() {{\n{}}}\npub fn call() {{ product(); }}\n",
        "let _ = 1;\n".repeat(3001)
    ));
    assert_eq!(f.count("common"), 3005);
    assert_eq!(f.report()["oversized_product_files"][0]["lines"], 3005);
    for expression in ["3 > 2", "3 < 4", "8 >> 1", "1 << 2"] {
        let f = Fixture::source(&format!(
            "enum Product {{\n#[cfg(test)]\nTest = {expression},\n#[cfg(unix)]\nProduction = 1,\n}}\n"
        ));
        assert_eq!(f.count("common"), 4, "{expression}");
        assert_eq!(violations(&f.report()).len(), 1);
    }
}

#[test]
fn b3_unsupported_inner_test_cfg_fails() {
    for item in ["fn f()", "impl Product", "trait Product", "extern \"C\""] {
        let f = Fixture::source(&format!("{item} {{\n#![cfg(all(test, unix))]\n}}\n"));
        assert!(measure(f.0.path(), ALLOWLIST).is_err(), "{item}");
    }
}

#[test]
fn b3_shebang_preserves_leading_file_test_cfg() {
    let f = Fixture::source(
        "#!/usr/bin/env rust-script\n#![cfg(test)]\n#[cfg(windows)]\npub fn only_test() {}\n",
    );
    assert_eq!(f.count("common"), 0);
    assert!(violations(&f.report()).is_empty());
    assert_eq!(
        Fixture::source("#! /* comment */ [allow(dead_code)]\nfn f() {}\n").count("common"),
        2
    );
}

#[test]
fn b3_deep_delimiters_do_not_overflow_default_stack() {
    let source = format!(
        "macro_rules! deep {{ () => {{ {}0{} }}; }}\n",
        "(".repeat(20_000),
        ")".repeat(20_000)
    );
    assert_eq!(Fixture::source(&source).count("common"), 1);
}

// Integration test and bench directories are test code as a whole, so the
// rules that locate test code inside a file do not apply to them.
#[test]
fn integration_directories_are_only_lexed() {
    for directory in ["tests", "benches"] {
        let f = Fixture::new();
        f.put(
            &format!("crates/kio-core/{directory}/integration.rs"),
            "mod common;\ndeclare!{mod common;}\nfn f() { #![cfg(test)] }\n",
        );
        assert!(measure(f.0.path(), ALLOWLIST).is_ok(), "{directory}");
    }
}

#[test]
fn b3_shebang_doc_comments_are_first_tokens() {
    for comment in ["/*! doc */", "/** doc */"] {
        let f = Fixture::source(&format!(
            "#! {comment} [cfg(test)]\n#[cfg(unix)] fn product() {{}}\n"
        ));
        assert_eq!(f.count("common"), 1, "{comment}");
        assert_eq!(violations(&f.report()).len(), 1);
    }
    for comment in ["//! doc", "/// doc"] {
        let f = Fixture::source(&format!(
            "#! {comment}\n[cfg(test)]\n#[cfg(unix)] fn product() {{}}\n"
        ));
        assert_eq!(f.count("common"), 2, "{comment}");
        assert_eq!(violations(&f.report()).len(), 1);
    }
    for comment in ["/* doc */", "/**/", "/*** doc */"] {
        assert_eq!(
            Fixture::source(&format!("#! {comment} [cfg(test)]\nfn f() {{}}\n")).count("common"),
            0,
            "{comment}"
        );
    }
    for comment in ["// ordinary", "//// ordinary"] {
        assert_eq!(
            Fixture::source(&format!("#! {comment}\n[cfg(test)]\nfn f() {{}}\n")).count("common"),
            0,
            "{comment}"
        );
    }
}
