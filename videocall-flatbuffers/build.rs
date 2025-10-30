use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=schemas/");
    
    let schema_dir = "schemas";
    let output_dir = "src/generated";

    // Create output directory
    std::fs::create_dir_all(output_dir).expect("Failed to create output directory");

    // Check if flatc is available
    let flatc_check = Command::new("flatc")
        .arg("--version")
        .output();

    if flatc_check.is_err() {
        eprintln!("Warning: flatc compiler not found. Skipping code generation.");
        eprintln!("Please install flatc: https://github.com/google/flatbuffers/releases");
        eprintln!("Or run 'make install-flatc' in the videocall-flatbuffers directory");
        return;
    }

    // Get all .fbs files
    let schema_paths = std::fs::read_dir(schema_dir)
        .expect("Failed to read schema directory")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? == "fbs" {
                Some(path)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    if schema_paths.is_empty() {
        eprintln!("Warning: No .fbs files found in {}", schema_dir);
        return;
    }

    // Compile each schema
    for schema_path in schema_paths {
        println!("cargo:rerun-if-changed={}", schema_path.display());
        
        let status = Command::new("flatc")
            .arg("--rust")
            .arg("--gen-mutable")
            .arg("--gen-object-api")
            .arg("--gen-onefile")
            .arg("-o")
            .arg(output_dir)
            .arg(&schema_path)
            .status()
            .expect("Failed to execute flatc");

        if !status.success() {
            panic!("Failed to compile schema: {}", schema_path.display());
        }
    }

    println!("FlatBuffer schemas compiled successfully");
}
