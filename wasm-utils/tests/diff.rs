use parity_wasm::elements;
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

fn slurp<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    let mut f = fs::File::open(path)?;
    let mut buf = vec![];
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

fn validate_wasm(binary: &[u8]) -> Result<(), wabt::Error> {
    wabt::Module::read_binary(&binary, &Default::default())?.validate()?;
    Ok(())
}

fn run_diff_test<F: FnOnce(&[u8]) -> Vec<u8>>(test_dir: &str, name: &str, test: F) {
    // FIXME: not going to work on windows?
    let mut fixture_path = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/",));
    fixture_path.push(test_dir);
    fixture_path.push(name);

    // FIXME: not going to work on windows?
    let mut expected_path =
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/expectations/"));
    expected_path.push(test_dir);
    expected_path.push(name);

    let fixture_wat = slurp(&fixture_path).expect("Failed to read fixture");
    let fixture_wasm = wabt::wat2wasm(fixture_wat).expect("Failed to read fixture");
    validate_wasm(&fixture_wasm).expect("Fixture is invalid");

    let expected_wat = slurp(&expected_path).unwrap_or_default();
    let expected_wat = String::from_utf8_lossy(&expected_wat);

    let actual_wasm = test(fixture_wasm.as_ref());
    validate_wasm(&actual_wasm).expect("Result module is invalid");

    let actual_wat = wabt::wasm2wat(&actual_wasm).expect("Failed to convert result wasm to wat");

    pretty_assertions::assert_str_eq!(actual_wat, expected_wat);
}

mod stack_height {
    use super::*;

    macro_rules! def_stack_height_test {
        ( $name:ident ) => {
            #[test]
            fn $name() {
                run_diff_test(
                    "stack-height",
                    concat!(stringify!($name), ".wat"),
                    |input| {
                        let module =
                            elements::deserialize_buffer(input).expect("Failed to deserialize");
                        let instrumented = wasm_utils::stack_height::inject_limiter(module, 1024)
                            .expect("Failed to instrument with stack counter");
                        elements::serialize(instrumented).expect("Failed to serialize")
                    },
                );
            }
        };
    }

    def_stack_height_test!(simple);
    def_stack_height_test!(start);
    def_stack_height_test!(table);
    def_stack_height_test!(global);
    def_stack_height_test!(imports);
}

mod gas {
    use super::*;

    macro_rules! def_gas_test {
        ( $name:ident ) => {
            #[test]
            fn $name() {
                run_diff_test("gas", concat!(stringify!($name), ".wat"), |input| {
                    let rules = wasm_utils::rules::Set::default();

                    let module =
                        elements::deserialize_buffer(input).expect("Failed to deserialize");
                    let instrumented = wasm_utils::inject_gas_counter(module, &rules, "env")
                        .expect("Failed to instrument with gas metering");
                    elements::serialize(instrumented).expect("Failed to serialize")
                });
            }
        };
    }

    def_gas_test!(ifs);
    def_gas_test!(simple);
    def_gas_test!(start);
    def_gas_test!(call);
    def_gas_test!(branch);
}
