use std::env;
use std::path::PathBuf;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut input: Option<PathBuf> = None;
    let mut output_dir: Option<PathBuf> = None;
    let mut contracts_rust_out: Option<PathBuf> = None;
    let mut contracts_csharp_out: Option<PathBuf> = None;
    let mut game_version: Option<String> = None;

    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "--input" if i + 1 < args.len() => {
                input = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--output-dir" if i + 1 < args.len() => {
                output_dir = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--contracts-rust-out" if i + 1 < args.len() => {
                contracts_rust_out = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--contracts-csharp-out" if i + 1 < args.len() => {
                contracts_csharp_out = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--game-version" if i + 1 < args.len() => {
                game_version = Some(args[i + 1].clone());
                i += 2;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: gen_metadata [--input <metadata.json> --output-dir <dir>] [--contracts-rust-out <file>] [--contracts-csharp-out <file>] [--game-version <ver>]"
                );
                return;
            }
            other => {
                eprintln!("Unknown argument: {other}");
                process::exit(2);
            }
        }
    }

    let wants_contracts = contracts_rust_out.is_some() || contracts_csharp_out.is_some();
    if let (Some(input_path), Some(out_dir)) = (&input, &output_dir) {
        match gen_metadata::generate_from_file(input_path, out_dir) {
            Ok(code) => {
                println!(
                    "Generated {} bytes into {}",
                    code.len(),
                    out_dir.join("generated_ids.rs").display()
                );
            }
            Err(err) => {
                eprintln!("gen_metadata failed: {err}");
                process::exit(1);
            }
        }
        if wants_contracts {
            let ver = match game_version {
                Some(v) => v,
                None => match gen_metadata::read_manifest_game_version(input_path) {
                    Ok(v) => v,
                    Err(err) => {
                        eprintln!("gen_metadata failed: {err}");
                        process::exit(1);
                    }
                },
            };
            if let Err(err) = gen_metadata::generate_contract_constants(
                &ver,
                contracts_rust_out.as_deref(),
                contracts_csharp_out.as_deref(),
            ) {
                eprintln!("gen_metadata failed: {err}");
                process::exit(1);
            }
        }
        return;
    }

    if wants_contracts {
        let ver = match (&game_version, &input) {
            (Some(v), _) => v.clone(),
            (None, Some(input_path)) => match gen_metadata::read_manifest_game_version(input_path) {
                Ok(v) => v,
                Err(err) => {
                    eprintln!("gen_metadata failed: {err}");
                    process::exit(1);
                }
            },
            (None, None) => {
                eprintln!("Missing required --game-version <ver> or --input <metadata.json> for contract generation");
                process::exit(2);
            }
        };
        if let Err(err) = gen_metadata::generate_contract_constants(
            &ver,
            contracts_rust_out.as_deref(),
            contracts_csharp_out.as_deref(),
        ) {
            eprintln!("gen_metadata failed: {err}");
            process::exit(1);
        }
        println!("Generated Rust/C# contract constants for {ver}");
        return;
    }

    eprintln!("Missing required arguments (--input/--output-dir or --contracts-*-out)");
    process::exit(2);
}
