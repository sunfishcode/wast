//! Finds as many tests as we can in the `wabt` submodule and does a few things:
//!
//! * First, asserts that we can parse and encode them all to binary.
//! * Next uses `wat2wasm` to encode to binary.
//! * Finally, asserts that the two binary encodings are byte-for-byte the same.
//!
//! This also has support for handling `*.wast` files from the official test
//! suite which involve parsing as a wast file and handling assertions. Also has
//! rudimentary support for running some of the assertions.

use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use wast_parser::parser::ParseBuffer;
use wast_parser::*;

fn main() {
    let tests = find_tests();
    let filter = std::env::args().nth(1);

    let tests = tests
        .par_iter()
        .filter_map(|test| {
            if let Some(filter) = &filter {
                if let Some(s) = test.file_name().and_then(|s| s.to_str()) {
                    if !s.contains(filter) {
                        return None;
                    }
                }
            }
            let contents = std::fs::read_to_string(test).unwrap();
            if skip_test(&test, &contents) {
                None
            } else {
                Some((test, contents))
            }
        })
        .collect::<Vec<_>>();

    println!("running {} tests\n", tests.len());

    let errors = tests
        .par_iter()
        .filter_map(|(test, contents)| run_test(test, contents).err())
        .collect::<Vec<_>>();

    if !errors.is_empty() {
        for msg in errors.iter() {
            eprintln!("{}", msg);
        }

        panic!("{} tests failed", errors.len())
    }

    println!("test result: ok. {} passed\n", tests.len());
}

fn run_test(test: &Path, contents: &str) -> anyhow::Result<()> {
    let wast = contents.contains("TOOL: wast2json")
        || contents.contains("TOOL: run-objdump-spec")
        || test.display().to_string().ends_with(".wast");
    if wast {
        return test_wast(test, contents);
    }
    let binary = wast::parse_file(test)?;

    // FIXME(#5) fix these tests
    if test.ends_with("invalid-elem-segment-offset.txt")
        || test.ends_with("invalid-data-segment-offset.txt")
    {
        return Ok(());
    }

    if let Some(expected) = wat2wasm(&test, None) {
        binary_compare(&test, &binary, &expected)?;
    }
    Ok(())
}

fn test_wast(test: &Path, contents: &str) -> anyhow::Result<()> {
    macro_rules! adjust {
        ($e:expr) => {{
            let mut e = wast_parser::Error::from($e);
            e.set_path(test);
            e.set_text(contents);
            e
        }};
    }
    let buf = ParseBuffer::new(contents).map_err(|e| adjust!(e))?;
    let wast = parser::parse::<Wast>(&buf).map_err(|e| adjust!(e))?;

    let mut modules = 0;
    for directive in wast.directives {
        match directive {
            WastDirective::Module(mut module) => {
                let actual = module.encode().map_err(|e| adjust!(e))?;

                match module.kind {
                    ModuleKind::Text(_) => {
                        if let Some(expected) = wat2wasm(&test, Some(modules)) {
                            binary_compare(&test, &actual, &expected)?;
                        }
                    }
                    // Skip these for the same reason we skip
                    // `module/binary-module.txt` in `binary_compare` below.
                    ModuleKind::Binary(_) => {}
                }

                modules += 1;
            }

            // FIXME(#13) run these tests
            WastDirective::AssertMalformed { message, .. }
                if message.starts_with("constant out of range") => {}

            WastDirective::AssertMalformed {
                span: _,
                module: QuoteModule::Quote(source),
                message,
            } => {
                let source = source.concat();
                let result = ParseBuffer::new(&source)
                    .map_err(|e| e.into())
                    .and_then(|b| -> Result<(), wast_parser::Error> {
                        let mut wat = parser::parse::<Wat>(&b)?;
                        wat.module.encode()?;
                        Ok(())
                    })
                    .map_err(|mut e| {
                        e.set_text(&source);
                        e
                    });
                match result {
                    Ok(()) => anyhow::bail!(
                        "\
                         in test {:?} parsed {:?} successfully\n\
                         but should have failed with: {}\
                         ",
                        test,
                        source,
                        message,
                    ),
                    Err(e) => {
                        if error_matches(&e.to_string(), message) {
                            continue;
                        }
                        anyhow::bail!(
                            "\
                             in test {:?} parsed {:?} with error: {}\n\
                             but should have failed with: {}\
                             ",
                            test,
                            source,
                            e,
                            message,
                        );
                    }
                }
            }
            _ => continue,
        }
    }

    Ok(())
}

fn error_matches(error: &str, message: &str) -> bool {
    if error.contains(message) {
        return true;
    }
    if message == "unknown operator" {
        return error.contains("expected a ") || error.contains("expected an ");
    }
    return false;
}

fn find_tests() -> Vec<PathBuf> {
    let mut tests = Vec::new();
    if !Path::new("tests/wabt").exists() {
        panic!("submodules need to be checked out");
    }
    find_tests("tests/wabt/test/desugar".as_ref(), &mut tests);
    find_tests("tests/wabt/test/dump".as_ref(), &mut tests);
    find_tests("tests/wabt/test/interp".as_ref(), &mut tests);
    find_tests("tests/wabt/test/parse".as_ref(), &mut tests);
    find_tests("tests/wabt/test/roundtrip".as_ref(), &mut tests);
    find_tests("tests/wabt/test/spec".as_ref(), &mut tests);
    find_tests("tests/wabt/test/typecheck".as_ref(), &mut tests);
    find_tests("tests/wabt/third_party/testsuite".as_ref(), &mut tests);
    find_tests("tests/regression".as_ref(), &mut tests);
    tests.sort();
    return tests;

    fn find_tests(path: &Path, tests: &mut Vec<PathBuf>) {
        for f in path.read_dir().unwrap() {
            let f = f.unwrap();
            if f.file_type().unwrap().is_dir() {
                find_tests(&f.path(), tests);
                continue;
            }

            match f.path().extension().and_then(|s| s.to_str()) {
                Some("txt") | Some("wast") | Some("wat") => {}
                _ => continue,
            }
            tests.push(f.path());
        }
    }
}

fn binary_compare(test: &Path, actual: &[u8], expected: &[u8]) -> Result<(), anyhow::Error> {
    use wasmparser::*;

    // I tried for a bit but honestly couldn't figure out a great way to match
    // wabt's encoding of the name section. Just remove it from our asserted
    // sections and don't compare against wabt's.
    let actual = remove_name_section(actual);

    // We test wabt with `--enable-all`, but this *always* emits a data count
    // section in the binary. We, however, only emit it if necessary. To handle
    // these differences remove wabt's data count section if our binary doesn't
    // have one.
    let expected = if contains_datacount_section(&actual) {
        expected.to_vec()
    } else {
        remove_datacount_section(expected)
    };

    if actual == expected {
        return Ok(());
    }

    let difference = actual
        .iter()
        .enumerate()
        .zip(&expected)
        .find(|((_, actual), expected)| actual != expected);
    let pos = match difference {
        Some(((pos, _), _)) => format!("at byte {} ({0:#x})", pos),
        None => format!("by being too small"),
    };
    let mut msg = format!(
        "
error: actual wasm differs {pos} from expected wasm
      --> {file}
",
        pos = pos,
        file = test.display(),
    );

    if let Some(((pos, _), _)) = difference {
        msg.push_str(&format!("  {:4} |   {:#04x}\n", pos - 2, actual[pos - 2]));
        msg.push_str(&format!("  {:4} |   {:#04x}\n", pos - 1, actual[pos - 1]));
        msg.push_str(&format!("  {:4} | - {:#04x}\n", pos, expected[pos]));
        msg.push_str(&format!("       | + {:#04x}\n", actual[pos]));
    }

    let mut actual_parser = Parser::new(&actual);
    let mut expected_parser = Parser::new(&expected);

    let mut differences = 0;
    let mut dots = 0;
    while differences < 5 && dots < 5 {
        let actual_state = match read_state(&mut actual_parser) {
            Some(s) => s,
            None => break,
        };
        let expected_state = match read_state(&mut expected_parser) {
            Some(s) => s,
            None => break,
        };

        if actual_state == expected_state {
            if differences > 0 {
                dots += 1;
                msg.push_str(&format!("       |   ...\n"));
            }
            continue;
        }

        if differences == 0 {
            msg.push_str("\n\n");
        }
        msg.push_str(&format!("       | - {}\n", expected_state));
        msg.push_str(&format!("       | + {}\n", actual_state));
        differences += 1;
    }

    anyhow::bail!("{}", msg);

    fn read_state<'a, 'b>(parser: &'b mut Parser<'a>) -> Option<String> {
        loop {
            match parser.read() {
                // ParserState::BeginSection { code: SectionCode::DataCount, .. } => {}
                // ParserState::DataCountSectionEntry(_) => {}
                ParserState::Error(_) | ParserState::EndWasm => break None,
                other => break Some(format!("{:?}", other)),
            }
        }
    }

    fn contains_datacount_section(bytes: &[u8]) -> bool {
        if let Ok(mut r) = ModuleReader::new(bytes) {
            while let Ok(s) = r.read() {
                match s.code {
                    SectionCode::DataCount => return true,
                    _ => {}
                }
            }
        }
        false
    }

    fn remove_name_section(bytes: &[u8]) -> Vec<u8> {
        if let Ok(mut r) = ModuleReader::new(bytes) {
            loop {
                let start = r.current_position();
                if let Ok(s) = r.read() {
                    match s.code {
                        SectionCode::Custom { name: "name", .. } => {
                            let mut bytes = bytes.to_vec();
                            bytes.drain(start..s.range().end);
                            return bytes;
                        }
                        _ => {}
                    }
                } else {
                    break;
                }
            }
        }
        return bytes.to_vec();
    }

    fn remove_datacount_section(bytes: &[u8]) -> Vec<u8> {
        if let Ok(mut r) = ModuleReader::new(bytes) {
            loop {
                let start = r.current_position();
                if let Ok(s) = r.read() {
                    match s.code {
                        SectionCode::DataCount => {
                            let mut bytes = bytes.to_vec();
                            bytes.drain(start..s.range().end);
                            return bytes;
                        }
                        _ => {}
                    }
                } else {
                    break;
                }
            }
        }
        return bytes.to_vec();
    }
}

fn wat2wasm(test: &Path, module: Option<usize>) -> Option<Vec<u8>> {
    if let Some(module) = module {
        let td = tempfile::TempDir::new().unwrap();
        let result = Command::new("wast2json")
            .arg(test)
            .arg("--enable-all")
            .arg("--no-check")
            .arg("-o")
            .arg(td.path().join("foo.json"))
            .output()
            .expect("failed to spawn `wat2wasm`");
        if !result.status.success() {
            // TODO: handle this case better
            return None;
        }
        let json = fs::read_to_string(td.path().join("foo.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        let commands = json["commands"].as_array().unwrap();
        let module = commands
            .iter()
            .filter_map(|m| {
                if m["type"] == "module" {
                    Some(td.path().join(m["filename"].as_str().unwrap()))
                } else {
                    None
                }
            })
            .skip(module)
            .next()
            .expect("failed to find right module");
        Some(fs::read(module).unwrap())
    } else {
        let f = tempfile::NamedTempFile::new().unwrap();
        let result = Command::new("wat2wasm")
            .arg(test)
            .arg("--enable-all")
            .arg("--no-check")
            .arg("-o")
            .arg(f.path())
            .output()
            .expect("failed to spawn `wat2wasm`");
        if result.status.success() {
            Some(fs::read(f.path()).unwrap())
        } else {
            // TODO: handle this case better
            None
        }
    }
}

fn skip_test(test: &Path, contents: &str) -> bool {
    // This test still uses a bunch of old names and I don't feel like
    // typing them all out at this time, so just skip it. We get some
    // testing from wabt's test suite anyway.
    if test.ends_with("threads/atomic.wast") {
        return true;
    }
    // The current SIMD spec and wabt seem to disagree about this test, let's
    // ignore it while the spec settles
    if test.ends_with("interp/simd-load-store.txt") {
        return true;
    }

    // TODO: need to fix this test, how in the world is `if` supposed to
    // be parsed anyway?
    if test.ends_with("dump/br-loop-inner.txt") {
        return true;
    }

    // FIXME(WebAssembly/wabt#1187) on macos this appears to be incorrect with
    // wabt, although still waiting on that issue itself.
    if test.ends_with("bulk-memory-named.txt") && cfg!(target_os = "macos") {
        return true;
    }

    // FIXME(#13) we're shielding ourselves from platform differences in hex
    // float parsing for now, but we should implement a Rust version that works
    // everywhere
    if test.ends_with("const.wast") && !cfg!(target_os = "linux") {
        return true;
    }

    // Skip tests that are supposed to fail
    if contents.contains(";; ERROR") {
        return true;
    }
    // These tests are acually ones that run with the `*.wast` files from the
    // official test suite, and we slurp those up elsewhere anyway.
    if contents.contains("STDIN_FILE") {
        return true;
    }
    // Some exception-handling tests don't use `--enable-exceptions` since
    // `run-objdump` enables everything
    if contents.contains("run-objdump") && contents.contains("(event") {
        return true;
    }

    // Skip tests that exercise unimplemented proposals
    if contents.contains("--enable-exceptions") {
        return true;
    }
    if contents.contains("--enable-all") {
        return true;
    }
    if contents.contains("--enable-annotations") {
        return true;
    }
    if contents.contains("--enable-tail-call") {
        return true;
    }
    false
}
