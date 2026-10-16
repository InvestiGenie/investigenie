use std::{env, fs, path::PathBuf};

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("../integrations/data_connectors");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut integrations = Vec::new();
    for entry in fs::read_dir(&root).expect("Cannot read integrations directory") {
        let path = entry.expect("Cannot read integration entry").path();
        if !path.is_dir() || !path.join("mod.rs").is_file() {
            continue;
        }
        let name = path
            .file_name()
            .unwrap()
            .to_str()
            .expect("Integration name must be UTF-8");
        assert!(
            name.chars().enumerate().all(|(position, character)| {
                character == '_'
                    || character.is_ascii_alphabetic()
                    || (position > 0 && character.is_ascii_digit())
            }),
            "Integration folder must be a Rust identifier: {name}"
        );
        integrations.push((name.to_owned(), path.join("mod.rs").canonicalize().unwrap()));
    }
    integrations.sort_by(|left, right| left.0.cmp(&right.0));
    let mut generated = String::new();
    for (name, path) in &integrations {
        generated.push_str(&format!(
            "#[path = {:?}]\npub mod {name};\n",
            path.to_str().unwrap()
        ));
    }
    generated.push_str("pub fn registered() -> Vec<IntegrationDescriptor> { vec![\n");
    for (name, _) in &integrations {
        generated.push_str(&format!("{name}::descriptor(),\n"));
    }
    generated.push_str("] }\n");
    generated.push_str("pub fn validate_registry() { let descriptors = registered(); let mut ids = std::collections::HashSet::new(); for descriptor in &descriptors { assert!(ids.insert(descriptor.id), \"Duplicate integration id\"); }\n");
    for (name, _) in &integrations {
        generated.push_str(&format!("assert_eq!({name}::descriptor().id, {name:?}, \"Integration id must match folder\");\n"));
    }
    generated.push_str("}\npub fn connect(id: &str, config: &ConnectionConfig) -> Result<std::sync::Arc<dyn Connector>, ConnectorError> { validate_registry(); match id {\n");
    for (name, _) in &integrations {
        generated.push_str(&format!("{name:?} => {name}::connect(config),\n"));
    }
    generated.push_str("_ => Err(ConnectorError::invalid_connection()), } }\n");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("data_connectors.rs");
    fs::write(output, generated).expect("Cannot write integration registry");
}
