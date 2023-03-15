use std::env;

fn main() {
    wasm_utils::logger::init();

    let args = env::args().collect::<Vec<_>>();
    if args.len() != 3 {
        println!("Usage: {} input_file.wasm output_file.wasm", args[0]);
        return;
    }

    // Loading module
    let module =
        parity_wasm::deserialize_file(&args[1]).expect("Module deserialization to succeed");

    let result = wasm_utils::inject_gas_counter(module, &wasm_utils::rules::Set::default(), "env")
        .expect("Failed to inject gas. Some forbidden opcodes?");

    parity_wasm::serialize_to_file(&args[2], result).expect("Module serialization to succeed")
}
