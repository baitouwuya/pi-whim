use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use walkdir::WalkDir;

#[derive(Clone)]
struct Record {
    provider: String,
    id: String,
    name: String,
    reasoning: bool,
    supports_images: bool,
    thinking_level_map: Vec<(String, Option<String>)>,
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let catalog_root = manifest.join("../../vendor/pi-mono/packages/ai/src/providers/data");
    println!("cargo:rerun-if-changed={}", catalog_root.display());
    println!("cargo:rerun-if-changed=build.rs");

    if !catalog_root.is_dir() {
        panic!(
            "pi-whim-core build: expected model catalog at {}, but it is not a directory. \
             Ensure vendor/pi-mono is checked out (e.g. `git submodule update --init \
             --recursive` or resync the vendored tree).",
            catalog_root.display()
        );
    }

    let mut records = BTreeMap::<(String, String), Record>::new();
    let mut files_seen = 0usize;
    for entry in WalkDir::new(&catalog_root)
        .max_depth(1)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            let path = entry.path();
            let is_json = path.extension().and_then(|value| value.to_str()) == Some("json");
            // Skip dotfiles (e.g. `.manifest.json`, `.DS_Store`): they are catalog
            // metadata/checksum files, not model records. A real catalog file missing
            // the required `id`/`provider` fields still fails the build below.
            let is_hidden = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'));
            is_json && !is_hidden
        })
    {
        files_seen += 1;
        read_catalog_file(entry.path(), &mut records);
    }

    if files_seen == 0 {
        panic!(
            "pi-whim-core build: no *.json catalog files found under {}. \
             The vendored model catalog appears empty; resync vendor/pi-mono.",
            catalog_root.display()
        );
    }

    if records.is_empty() {
        panic!(
            "pi-whim-core build: read {} catalog file(s) under {} but produced zero model \
             records. The provider data schema may have drifted; inspect the catalog or \
             update build.rs to match the new shape.",
            files_seen,
            catalog_root.display(),
        );
    }

    let output = render_catalog(records.into_values());
    let output_path = PathBuf::from(env::var("OUT_DIR").expect("build output directory"))
        .join("bundled_model_capabilities.rs");
    fs::write(output_path, output).expect("write bundled model capability index");
}

fn read_catalog_file(path: &Path, records: &mut BTreeMap<(String, String), Record>) {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) => panic!(
            "pi-whim-core build: failed to read catalog {}: {}",
            path.display(),
            err
        ),
    };
    let parsed = match serde_json::from_str::<Value>(&contents) {
        Ok(value) => value,
        Err(err) => panic!(
            "pi-whim-core build: catalog {} is not valid JSON: {}",
            path.display(),
            err
        ),
    };
    let Value::Object(models) = parsed else {
        panic!(
            "pi-whim-core build: catalog {} is not a JSON object at its root. \
             The provider data schema may have drifted.",
            path.display()
        );
    };

    for value in models.values() {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            panic!(
                "pi-whim-core build: catalog {} contains a model entry missing the required \
                 field `id`. The provider data schema may have drifted; regenerate the \
                 catalog or update build.rs to match the new shape.",
                path.display()
            );
        };
        let Some(provider) = value.get("provider").and_then(Value::as_str) else {
            panic!(
                "pi-whim-core build: catalog {} contains model entry `{:?}` missing the \
                 required field `provider`. The provider data schema may have drifted; \
                 regenerate the catalog or update build.rs to match the new shape.",
                path.display(),
                id,
            );
        };
        // Optional fields keep safe defaults; a missing optional field is not schema drift.
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned();
        let reasoning = value
            .get("reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let supports_images = value
            .get("input")
            .and_then(Value::as_array)
            .is_some_and(|inputs| inputs.iter().any(|input| input.as_str() == Some("image")));
        let mut thinking_level_map = value
            .get("thinkingLevelMap")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(level, mapped)| (level.clone(), mapped.as_str().map(str::to_owned)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        thinking_level_map.sort_by(|left, right| left.0.cmp(&right.0));
        let record = Record {
            provider: provider.to_owned(),
            id: id.to_owned(),
            name,
            reasoning,
            supports_images,
            thinking_level_map,
        };
        records.insert((provider.to_owned(), id.to_owned()), record);
    }
}

fn render_catalog(records: impl Iterator<Item = Record>) -> String {
    let mut output = String::from(
        "// Generated from vendor/pi-mono provider data. Do not edit.\n\
         pub(super) static BUNDLED_CAPABILITIES: &[BundledCapability] = &[\n",
    );
    for record in records {
        let map = record
            .thinking_level_map
            .iter()
            .map(|(level, value)| match value {
                Some(value) => format!("({level:?}, Some({value:?}))"),
                None => format!("({level:?}, None)"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            "    BundledCapability {{ provider: {:?}, id: {:?}, name: {:?}, reasoning: {}, supports_images: {}, thinking_level_map: &[{}] }},\n",
            record.provider,
            record.id,
            record.name,
            record.reasoning,
            record.supports_images,
            map,
        ));
    }
    output.push_str("];\n");
    output
}
