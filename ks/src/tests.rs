//! Host tests: an in-memory disk stands in for the kernel.

use std::collections::BTreeMap;
use std::string::{String, ToString};
use std::vec::Vec;

use crate::{display, Engine, FileInfo, Host, Value};

#[derive(Default)]
struct MemHost {
    /// Absolute path -> contents (`None` for a folder).
    files: BTreeMap<String, Option<Vec<u8>>>,
    cwd: String,
    out: String,
    answer: bool,
    asked: Vec<String>,
}

impl MemHost {
    fn new() -> MemHost {
        let mut h = MemHost { cwd: "/".into(), answer: true, ..Default::default() };
        h.files.insert("/".into(), None);
        h.put("/DOOM1.WAD", &vec![0u8; 4_196_020]);
        h.put("/notes.txt", b"hello\nworld\n");
        h.put("/big.bin", &vec![1u8; 60_000_000]);
        h.files.insert("/docs".into(), None);
        h.put("/docs/a.md", b"# A");
        h.put("/docs/b.md", b"# B, longer");
        h.put("/data.json", br#"{"name": "konjac", "tags": ["os", "rust"], "size": 3, "nested": {"x": 1.5}}"#);
        h
    }

    fn put(&mut self, p: &str, d: &[u8]) {
        self.files.insert(p.into(), Some(d.to_vec()));
    }

    fn info(&self, abs: &str) -> Option<FileInfo> {
        let e = self.files.get(abs)?;
        let name = abs.rsplit('/').next().unwrap_or("").to_string();
        Some(FileInfo { name, is_dir: e.is_none(), size: e.as_ref().map(|d| d.len() as u64).unwrap_or(0), modified: Some(1_759_000_000_000_000_000), created: None })
    }
}

impl Host for MemHost {
    fn print(&mut self, text: &str) {
        self.out.push_str(text);
    }
    fn cwd(&mut self) -> String {
        self.cwd.clone()
    }
    fn set_cwd(&mut self, path: &str) -> Result<(), String> {
        let a = self.absolute(path);
        match self.files.get(&a) {
            Some(None) => {
                self.cwd = a;
                Ok(())
            }
            Some(Some(_)) => Err("not a directory".into()),
            None => Err("not found".into()),
        }
    }
    fn absolute(&mut self, path: &str) -> String {
        let base = if path.starts_with('/') { String::new() } else { self.cwd.clone() };
        let mut parts: Vec<&str> = Vec::new();
        for p in base.split('/').chain(path.split('/')) {
            match p {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                p => parts.push(p),
            }
        }
        let mut s = String::new();
        for p in parts {
            s.push('/');
            s.push_str(p);
        }
        if s.is_empty() {
            s.push('/');
        }
        s
    }
    fn list_dir(&mut self, path: &str) -> Result<Vec<FileInfo>, String> {
        let a = self.absolute(path);
        if self.files.get(&a) != Some(&None) {
            return Err("not a directory".into());
        }
        let prefix = if a == "/" { "/".to_string() } else { a.clone() + "/" };
        let names: Vec<String> = self.files.keys().filter(|k| k.starts_with(&prefix) && k.len() > prefix.len() && !k[prefix.len()..].contains('/')).cloned().collect();
        Ok(names.iter().filter_map(|n| self.info(n)).collect())
    }
    fn stat(&mut self, path: &str) -> Result<FileInfo, String> {
        let a = self.absolute(path);
        self.info(&a).ok_or_else(|| "not found".into())
    }
    fn read(&mut self, path: &str) -> Result<Vec<u8>, String> {
        let a = self.absolute(path);
        match self.files.get(&a) {
            Some(Some(d)) => Ok(d.clone()),
            Some(None) => Err("is a directory".into()),
            None => Err("not found".into()),
        }
    }
    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), String> {
        let a = self.absolute(path);
        self.files.insert(a, Some(data.to_vec()));
        Ok(())
    }
    fn remove(&mut self, path: &str) -> Result<(), String> {
        let a = self.absolute(path);
        if a == "/fat" {
            return Err("that's where a disk is attached".into());
        }
        let prefix = a.clone() + "/";
        self.files.retain(|k, _| *k != a && !k.starts_with(&prefix));
        Ok(())
    }
    fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
        let (a, b) = (self.absolute(from), self.absolute(to));
        let d = self.files.remove(&a).ok_or("not found")?;
        self.files.insert(b, d);
        Ok(())
    }
    fn copy(&mut self, from: &str, to: &str) -> Result<(), String> {
        let (a, b) = (self.absolute(from), self.absolute(to));
        let d = self.files.get(&a).cloned().ok_or("not found")?;
        self.files.insert(b, d);
        Ok(())
    }
    fn mkdir(&mut self, path: &str) -> Result<(), String> {
        let a = self.absolute(path);
        self.files.insert(a, None);
        Ok(())
    }
    fn now(&mut self) -> i64 {
        1_759_752_000_000_000_000
    }
    fn confirm(&mut self, q: &str) -> bool {
        self.asked.push(q.to_string());
        self.answer
    }
    fn interrupted(&mut self) -> bool {
        false
    }
    fn apex(&mut self, _: &str) -> bool {
        true
    }
    fn width(&mut self) -> usize {
        88
    }
}

fn run(e: &mut Engine, h: &mut MemHost, src: &str) -> Value {
    match e.run(src, h) {
        Ok(v) => v,
        Err(err) => panic!("{src}\n{}", e.render_error(&err)),
    }
}

fn shown(src: &str) -> String {
    let (mut e, mut h) = (Engine::new(), MemHost::new());
    let v = run(&mut e, &mut h, src);
    display::render(&v, 88)
}

fn err(src: &str) -> String {
    let (mut e, mut h) = (Engine::new(), MemHost::new());
    match e.run(src, &mut h) {
        Ok(v) => panic!("{src} should fail, gave {v:?}"),
        Err(x) => e.render_error(&x),
    }
}

#[test]
fn literals() {
    assert_eq!(shown("50MB"), "50.0 MB");
    assert_eq!(shown("4KiB"), "4.1 KB");
    assert_eq!(shown("4KiB | into int"), "4096");
    assert_eq!(shown("1.5GB | into int"), "1500000000");
    assert_eq!(shown("90s"), "1min 30s");
    assert_eq!(shown("2026-10-06"), "2026-10-06 00:00:00");
    assert_eq!(shown("2026-10-06T14:30 + 90min"), "2026-10-06 16:00:00");
    assert_eq!(shown("0x1F"), "31");
    assert_eq!(shown("1e3"), "1000.0");
    assert_eq!(shown("-5"), "-5");
}

#[test]
fn arithmetic() {
    assert_eq!(shown("1 + 2 * 3"), "7");
    assert_eq!(shown("(1 + 2) * 3"), "9");
    assert_eq!(shown("7 / 2"), "3.5");
    assert_eq!(shown("8 / 2"), "4");
    assert_eq!(shown("7 mod 3"), "1");
    assert_eq!(shown("1MB + 500KB"), "1.5 MB");
    assert_eq!(shown("10MB / 4"), "2.5 MB");
    assert_eq!(shown("10MB / 5MB"), "2.0");
    assert_eq!(shown("2026-10-07 - 2026-10-06"), "1day");
    assert_eq!(shown("\"a\" + \"b\""), "ab");
    assert_eq!(shown("[1 2] ++ [3]"), "#  value\n-  -----\n0      1\n1      2\n2      3");
    assert_eq!(shown("1 < 2 and 3 > 4"), "false");
    assert_eq!(shown("not (1 == 2)"), "true");
    assert_eq!(shown("2 in [1 2 3]"), "true");
    assert_eq!(shown("\"DOOM1.WAD\" =~ \"*.wad\""), "true");
}

#[test]
fn unit_errors() {
    let e = err("5MB > 5");
    assert!(e.contains("can't compare a size with an int"), "{e}");
    assert!(e.contains("5 has no unit; did you mean 5MB?"), "{e}");
    assert!(err("1 + \"a\"").contains("can't add an int with a string"));
    assert!(err("9223372036854775807 + 1").contains("too big"));
    assert!(err("1 / 0").contains("division by zero"));
}

#[test]
fn ls_filter_sort() {
    let (mut e, mut h) = (Engine::new(), MemHost::new());
    let v = run(&mut e, &mut h, "ls | filter size > 1MB | sort-by size | get name");
    assert_eq!(v.to_text(), "[DOOM1.WAD, big.bin]");
    let v = run(&mut e, &mut h, "ls | filter type == dir | get name");
    assert_eq!(v.to_text(), "[docs]");
    let v = run(&mut e, &mut h, "ls docs | get name");
    assert_eq!(v.to_text(), "[docs/a.md, docs/b.md]");
    let v = run(&mut e, &mut h, "ls *.json | length");
    assert_eq!(v.to_text(), "1");
    let v = run(&mut e, &mut h, "ls | filter name =~ \"*.wad\" or name == notes.txt | length");
    assert_eq!(v.to_text(), "2");
    let v = run(&mut e, &mut h, "ls | sort-by size -r | first | get name");
    assert_eq!(v.to_text(), "big.bin");
    let v = run(&mut e, &mut h, "ls | filter {|f| $f.size < 100B } | get name");
    assert_eq!(v.to_text(), "[docs, data.json, notes.txt]");
}

#[test]
fn table_display() {
    let t = shown("ls | select name size");
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines[0], "#  name          size");
    assert_eq!(lines[1], "-  ---------  -------");
    assert!(lines.iter().any(|l| l.contains("DOOM1.WAD") && l.ends_with("4.2 MB")), "{t}");
    assert_eq!(shown("{name: konjac, size: 3KB}"), "name  konjac\nsize  3.0 KB");
    assert_eq!(shown("[]"), "(empty list)");
}

#[test]
fn variables_and_closures() {
    let (mut e, mut h) = (Engine::new(), MemHost::new());
    run(&mut e, &mut h, "let limit = 1MB");
    assert_eq!(run(&mut e, &mut h, "ls | filter size > $limit | length").to_text(), "2");
    assert_eq!(run(&mut e, &mut h, "mut n = 0; for f in (ls) { n = $n + 1 }; $n").to_text(), "5");
    assert_eq!(run(&mut e, &mut h, "[1 2 3] | each {|x| $x * 10 } | math sum").to_text(), "60");
    assert_eq!(run(&mut e, &mut h, "[a b] | each { str upcase }").to_text(), "[A, B]");
    assert_eq!(run(&mut e, &mut h, "let f = {|x| $x + $limit }; do $f 1MB").to_text(), "2.0 MB");
    assert!(err("let x = 1; x = 2").contains("was made with `let`"));
    assert!(err("$nope").contains("there's no variable `$nope`"));
}

#[test]
fn defs_and_control_flow() {
    let (mut e, mut h) = (Engine::new(), MemHost::new());
    run(&mut e, &mut h, "def big [folder: string = \".\", --over: size = 10MB] { ls $folder | filter size > $over | get name }");
    assert_eq!(run(&mut e, &mut h, "big").to_text(), "[big.bin]");
    assert_eq!(run(&mut e, &mut h, "big --over 1MB").to_text(), "[big.bin, DOOM1.WAD]");
    run(&mut e, &mut h, "def fact [n: int] { if $n <= 1 { 1 } else { $n * (fact ($n - 1)) } }");
    assert_eq!(run(&mut e, &mut h, "fact 10").to_text(), "3628800");
    assert_eq!(run(&mut e, &mut h, "mut i = 0; while true { i = $i + 1; if $i == 5 { break } }; $i").to_text(), "5");
    assert_eq!(run(&mut e, &mut h, "if 1 > 2 { 'a' } else if 2 > 1 { 'b' } else { 'c' }").to_text(), "b");
    let r = e.run("fact abc", &mut h).unwrap_err();
    let x = e.render_error(&r);
    assert!(x.contains("<n> should be an int, not a string"), "{x}");
    run(&mut e, &mut h, "def forever [] { forever }");
    let r = e.run("forever", &mut h).unwrap_err();
    assert!(r.msg.contains("too many calls"), "{}", r.msg);
}

#[test]
fn strings() {
    assert_eq!(shown("let name = konjac; \"hi $name, (2 + 3) things\""), "hi konjac, 5 things");
    assert_eq!(shown("open notes.txt | lines | length"), "2");
    assert_eq!(shown("\"a,b,c\" | split \",\" | str join -"), "a-b-c");
    assert_eq!(shown("\"Hello\" | str contains -i hell"), "true");
    assert_eq!(shown("\"  x  \" | str trim | str length"), "1");
    assert_eq!(shown("'raw $x'"), "raw $x");
    assert_eq!(shown("\"cost \\$5\""), "cost $5");
}

#[test]
fn json() {
    assert_eq!(shown("open data.json | get tags.1"), "rust");
    assert_eq!(shown("open data.json | get nested.x"), "1.5");
    assert_eq!(shown("{a: [1 2], b: null} | to json -r"), r#"{"a":[1,2],"b":null}"#);
    assert_eq!(shown("'{\"k\": \"\\u00e9\"}' | from json | get k"), "\u{e9}");
    assert!(err("'{\"k\": }' | from json").contains("bad JSON"));
}

#[test]
fn delete_asks_and_is_all_or_nothing() {
    let (mut e, mut h) = (Engine::new(), MemHost::new());
    // A missing file stops it before anything is deleted.
    let r = e.run("delete notes.txt missing.txt", &mut h).unwrap_err();
    assert!(r.msg.contains("missing.txt: not found"), "{}", r.msg);
    assert!(h.files.contains_key("/notes.txt"));
    // Piped: asks, with the count and size.
    h.answer = false;
    run(&mut e, &mut h, "ls | filter size > 1MB | delete");
    assert_eq!(h.asked, vec!["delete 2 files (64.2 MB)?".to_string()]);
    assert!(h.files.contains_key("/big.bin"));
    h.answer = true;
    run(&mut e, &mut h, "ls | filter size > 1MB | delete");
    assert!(!h.files.contains_key("/big.bin") && !h.files.contains_key("/DOOM1.WAD"));
    // One file named: doesn't ask.
    h.asked.clear();
    run(&mut e, &mut h, "rm notes.txt");
    assert!(h.asked.is_empty() && !h.files.contains_key("/notes.txt"));
    // --dry-run lists and stops.
    let v = run(&mut e, &mut h, "ls docs | delete --dry-run | length");
    assert_eq!(v.to_text(), "2");
    assert!(h.files.contains_key("/docs/a.md"));
}

#[test]
fn move_and_copy() {
    let (mut e, mut h) = (Engine::new(), MemHost::new());
    run(&mut e, &mut h, "ls *.json | copy docs/");
    assert!(h.files.contains_key("/docs/data.json") && h.files.contains_key("/data.json"));
    run(&mut e, &mut h, "mv notes.txt docs");
    assert!(h.files.contains_key("/docs/notes.txt") && !h.files.contains_key("/notes.txt"));
    assert!(err("cp big.bin DOOM1.WAD").contains("already there"));
    run(&mut e, &mut h, "\"hi\" | save new.txt; cd docs; pwd");
    assert_eq!(run(&mut e, &mut h, "pwd").to_text(), "/docs");
    assert_eq!(run(&mut e, &mut h, "open ../new.txt").to_text(), "hi");
    assert!(e.run("\"x\" | save ../new.txt", &mut h).unwrap_err().msg.contains("already a file"));
}

#[test]
fn parse_errors_point_at_the_problem() {
    let x = err("ls | sort-by size --rev");
    assert!(x.contains("sort-by has no flag --rev"), "{x}");
    assert!(x.contains("flags: --reverse"), "{x}");
    let x = err("lss");
    assert!(x.contains("unknown command `lss`") && x.contains("did you mean `ls`?"), "{x}");
    let x = err("str");
    assert!(x.contains("str contains"), "{x}");
    assert!(err("\"open").contains("no closing"));
    assert!(err("ls | get").contains("get needs <path>"));
    assert!(err("ls | filter size > 5").contains("did you mean 5MB?"));
    // The caret sits under the comparison.
    let x = err("ls | filter size > 5");
    let lines: Vec<&str> = x.lines().collect();
    assert_eq!(lines[1], "  | ls | filter size > 5");
    assert_eq!(lines[2], "  |             ^^^^^^^^ 5 has no unit; did you mean 5MB?");
}

#[test]
fn errors_name_the_item() {
    let x = err("[1 2 x] | each {|v| $v * 2 }");
    assert!(x.contains("each: can't multiply a string with an int (item 3 of 3)"), "{x}");
}

#[test]
fn never_panics_on_garbage() {
    // Random byte soup, and every prefix of some real lines.
    let samples = [
        "ls | filter size > 50MB and name =~ \"*.wad\" | sort-by modified -r | first 3",
        "def f [a: int, --b: size = 1MB, ...rest] { if $a > 0 { $a } else { f ($a + 1) } }",
        "{a: [1 {b: \"x $y (1 + (2 * 3))\"}], 'c d': -$z}",
        "let x = (ls | each {|r| $r.name | str upcase } | str join \", \")",
    ];
    let (mut e, mut h) = (Engine::new(), MemHost::new());
    for s in samples {
        for i in 0..=s.len() {
            if s.is_char_boundary(i) {
                let _ = e.run(&s[..i], &mut h);
            }
        }
    }
    let mut x: u64 = 0x1234_5678;
    let alphabet = b"ls|{}[]()\"'$.,:=<>-+*/ 0123456789abcMB\n#\\fiter";
    for _ in 0..3000 {
        let mut line = String::new();
        for _ in 0..(x % 40) {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            line.push(alphabet[(x >> 33) as usize % alphabet.len()] as char);
        }
        let _ = e.run(&line, &mut h);
        x = x.wrapping_add(1);
    }
    // Deep nesting is an error, not a stack overflow.
    let deep = "(".repeat(500) + &")".repeat(500);
    assert!(e.run(&deep, &mut h).is_err());
}

#[test]
fn readme_examples() {
    let (mut e, mut h) = (Engine::new(), MemHost::new());
    run(&mut e, &mut h, "ls | filter size > 50MB and name =~ \"*.bin\" | sort-by modified -r");
    run(&mut e, &mut h, "let big = (ls | filter size > 1MB)");
    run(&mut e, &mut h, "def kb [file: string] { (stat $file).size / 1KB }");
    assert_eq!(run(&mut e, &mut h, "kb notes.txt").to_text(), "0.012");
    run(&mut e, &mut h, "\"hello\" | save hello.txt");
    assert_eq!(run(&mut e, &mut h, "open data.json | get tags.0").to_text(), "os");
    assert_eq!(run(&mut e, &mut h, "help | filter group == files | length").to_text(), "14");
    assert!(run(&mut e, &mut h, "help sort-by").to_text().contains("-r, --reverse"));
}
