use std::collections::{HashMap, HashSet};
use std::{ffi, fs, io};

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;

#[derive(Parser)]
struct Cli {
    #[arg(short, long, required = true, num_args = 1..)]
    files: Vec<std::path::PathBuf>,
}

// TODO: move error handling to eyre
fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    logger_init();

    // First, we open all JSON files. This short-circuits in case of any IO error
    let size_hint = Some(cli.files.len());
    let readers_and_paths = files_load(cli.files, size_hint)?;

    // Then, we parse all files to JSON, loading them into RAM
    let size_hint = Some(readers_and_paths.len());
    let mut json_in_and_files = json_load(readers_and_paths, size_hint)?;

    // With the files contents in memory, we can start grouping all components together. These are
    // stored along with the file they were declared in to support relative path dereferencing.
    let ref_map = ref_load(&json_in_and_files)?;

    // This is where the actual component de-referencing takes place, yielding a de-referenced map
    // of all components, stored with information on their file of origin.
    let deref_map = ref_resolved(&ref_map)?;

    // Now, we can finally go through and de-reference the rest of the JSON Schema
    for (json, path) in json_in_and_files.iter_mut() {
        let methods = json
            .get("methods")
            .with_context(|| format!("invalid Json RPC format in {path}: should have a 'methods' top-level field"))?;

        let serde_json::Value::Object(methods) = methods else {
            anyhow::bail!("Error parsing {path}: 'methods' is not a json object",);
        };

        let acc_inner = serde_json::Map::with_capacity(methods.len());
        let methods = methods
            .iter()
            .try_fold(acc_inner, |mut acc, (key, val)| {
                acc.insert(key.clone(), ref_replace(&path, val, &deref_map)?);
                anyhow::Ok(acc)
            })
            .map(|methods| serde_json::Value::Object(methods))?;

        json.insert("methods".to_string(), methods);
    }

    // Once the de-referencing has taken place, we now need to merge each JSON Schema into one
    let mut method_set = HashSet::<String>::default();
    let mut acc = serde_json::Map::default();
    acc.insert("methods".to_string(), serde_json::Value::Array(Vec::default()));
    let json_out = json_in_and_files.into_iter().try_fold(acc, |mut acc, (json, path)| {
        let methods = json
            .get("methods")
            .with_context(|| format!("Error parsing {path}: should have a 'methods' top-level field"))?;

        let serde_json::Value::Array(methods) = methods else {
            anyhow::bail!("Error parsing {path}: top-level field 'methods' should be a json array");
        };

        for method in methods {
            let name = method
                .as_object()
                .with_context(|| format!("Error parsing {path}: rpc methods should be a json object"))?
                .get("name")
                .with_context(|| format!("Error parsing {path}: methods should have a name"))?
                .as_str()
                .with_context(|| format!("Error parsing {path}: method name should be a string"))?;

            anyhow::ensure!(
                method_set.insert(name.to_string()),
                "Error merging {path}: method '{name}' has already been declared in a previous file"
            );

            acc.get_mut("methods")
                .expect("'methods' should have been added manually")
                .as_array_mut()
                .expect("'methods' should be an array")
                .push(method.clone());
        }

        anyhow::Ok(acc)
    })?;

    anyhow::Ok(())
}

fn logger_init() {
    #[cfg(not(test))]
    let level = tracing::level_filters::LevelFilter::INFO;
    #[cfg(test)]
    let level = tracing::level_filters::LevelFilter::TRACE;
    let fmt_layer = tracing_subscriber::fmt::layer().with_test_writer().pretty().with_filter(level);
    let subscriber = tracing_subscriber::registry().with(fmt_layer);

    if let Err(_) = tracing::subscriber::set_global_default(subscriber) {
        tracing::warn!("Attempted to set global subscriber again");
    }
}

#[tracing::instrument]
fn files_load<I>(iter: I, size_hint: Option<usize>) -> anyhow::Result<Vec<(io::BufReader<fs::File>, String)>>
where
    I: IntoIterator<Item = std::path::PathBuf> + std::fmt::Debug,
{
    tracing::info!("Loading files");

    iter.into_iter()
        .try_fold(Vec::with_capacity(size_hint.unwrap_or_default()), |mut acc, path| {
            tracing::debug!("Opening file: {}", path.to_string_lossy());

            anyhow::ensure!(
                path.extension().map(ffi::OsStr::to_str) == Some(Some("json")),
                "{} is not a json file",
                path.to_string_lossy()
            );

            tracing::debug!("Opening file: {} - SUCCESS", path.to_string_lossy());
            acc.push((io::BufReader::new(fs::File::open(&path)?), path.to_string_lossy().to_string()));

            anyhow::Ok(acc)
        })
        .context("Failed to open files")
}

#[tracing::instrument(skip(iter))]
fn json_load<I>(
    iter: I,
    size_hint: Option<usize>,
) -> anyhow::Result<Vec<(serde_json::Map<String, serde_json::Value>, String)>>
where
    I: IntoIterator<Item = (io::BufReader<fs::File>, String)> + std::fmt::Debug,
{
    tracing::info!("Loading file json");
    iter.into_iter()
        .try_fold(Vec::with_capacity(size_hint.unwrap_or_default()), |mut acc, (reader, path)| {
            tracing::debug!("Loading file json: {path}");
            let json = (serde_json::from_reader(reader).with_context(|| format!("Failed to read {path}")))?;
            tracing::debug!("Loading file json: {path} - SUCCESS");

            acc.push((json, path));
            anyhow::Ok(acc)
        })
        .context("Failed to read files to json")
}

#[tracing::instrument(skip(iter))]
fn ref_load<I>(iter: &I) -> anyhow::Result<HashMap<String, (String, serde_json::Value)>>
where
    I: ?Sized,
    for<'a> &'a I: IntoIterator<Item = &'a (serde_json::Map<String, serde_json::Value>, String)>
        + std::fmt::Debug
        + serde::Serialize,
{
    tracing::info!("Loading file references");
    tracing::trace!("File references are: {}", serde_json::to_string_pretty(&iter).unwrap());

    iter.into_iter().try_fold(HashMap::<String, (String, serde_json::Value)>::default(), |mut acc, (json, path)| {
        tracing::debug!("Loading references from file: {path}");
        tracing::debug!("Loading references from file: {path} - EXTRACTING SCHEMAS");

        let schemas = json
            .get("components")
            .with_context(|| format!("invalid Json RPC format in {path}: should have a 'components' top-level field"))?
            .as_object()
            .with_context(|| format!("Error parsing {path}: top-level field 'components' should be a json object"))?
            .get("schemas")
            .with_context(|| format!("Error parsing {path}: 'components/schemas' is missing"))?
            .as_object()
            .with_context(|| format!("Error parsing {path}: 'components/schemas' is not a json object"))?;

        tracing::debug!("Loading references from file: {path} - STORING SCHEMAS");

        for (key, value) in schemas.iter() {
            if !acc.contains_key(key) {
                let mut path_key = String::with_capacity(path.len() + key.len());
                path_key.push_str(path);
                path_key.push_str(key);

                tracing::debug!("Storing reference at key: {path_key}");
                acc.insert(path_key, (path.to_owned(), value.clone()));
            } else {
                anyhow::bail!(format!("Error parsing {path}: '{key}' component is a duplicate"));
            }
        }

        tracing::debug!("Loading references from file: {path} - SUCCESS");

        anyhow::Ok(acc)
    })
}

/// This is the heart of the program: it will recursively traverse a component, resolving any
/// sub-references down to a single component. Once this step is complete, we will have a fully
/// de-referenced map we can use to insert components into the rest of the schema
#[tracing::instrument(skip(val, ref_map))]
fn ref_resolve(
    local_file: &str,
    val: &serde_json::Value,
    ref_map: &HashMap<String, (String, serde_json::Value)>,
) -> anyhow::Result<serde_json::Value> {
    tracing::info!("Resolving reference");
    tracing::trace!("Reference is: {}", serde_json::to_string_pretty(&val).unwrap());
    tracing::debug!("Asserting reference type");

    match val {
        serde_json::Value::Object(ref object) => {
            tracing::debug!("Asserting reference type - OBJECT");

            let object = object.into_iter().try_fold(serde_json::Map::default(), |mut acc, (key_outer, val)| {
                tracing::debug!("Checking for nested reference, key is {key_outer}");

                if key_outer == "$ref" {
                    if let serde_json::Value::String(ref_name) = val {
                        tracing::debug!("Found a nested reference: {ref_name}");

                        if let serde_json::Value::Object(deref_val) = ref_resolve(local_file, val, ref_map)? {
                            for (key_inner, val) in deref_val {
                                anyhow::ensure!(
                                    acc.insert(key_inner.clone(), val.clone()).is_none(),
                                    "Error parsing {local_file}: '{key_inner}' is overwritten multiple times in \
                                     {key_outer}"
                                );
                            }
                        } else {
                            anyhow::bail!(
                                "Invalid reference component {}, components must be a JSON object",
                                val.as_str().unwrap()
                            );
                        }
                    }
                } else if matches!(val, serde_json::Value::Object(_)) {
                    acc.insert(key_outer.clone(), ref_resolve(local_file, val, ref_map)?);
                } else {
                    acc.insert(key_outer.clone(), val.clone());
                }

                anyhow::Ok(acc)
            })?;

            tracing::debug!("Resolving reference - SUCCESS");
            anyhow::Ok(serde_json::Value::Object(object))
        }
        serde_json::Value::String(ref_path) => {
            tracing::debug!("Asserting reference type - NESTED REFERENCE");
            tracing::debug!("Extracting reference path");

            anyhow::ensure!(ref_path.len() > 20, "Error parsing {local_file}: invalid reference format {ref_path}");
            let (ref_file, ref_name) = ref_path
                .split_once("#")
                .map(|(l, r)| (l.trim_end_matches('/'), &r[19..]))
                .with_context(|| format!("Error parsing {local_file}: invalid reference format {ref_path}"))?;

            tracing::debug!("Extracting reference path - SUCCESS");
            tracing::debug!("Extracting reference key");

            let span = tracing::debug_span!("Nested reference key", ref_file, local_file, ref_name).entered();
            let key = if ref_file == "" {
                tracing::debug!("Reference is local");
                let mut key = String::with_capacity(local_file.len() + ref_name.len());
                key.push_str(local_file);
                key.push_str(ref_name);
                key
            } else {
                tracing::debug!("Reference was declared in a separate file");
                let mut key = String::with_capacity(ref_file.len() + ref_name.len());
                key.push_str(ref_file);
                key.push_str(ref_name);
                key
            };
            span.exit();

            tracing::debug!("Extracting reference key: {key}");

            let _span = tracing::debug_span!("Resolving nested reference", key).entered();
            tracing::trace!("Reference set is: {}", serde_json::to_string_pretty(&ref_map).unwrap());
            tracing::debug!("Looking for reference in reference map");
            let (ref_file, mut ref_val) = ref_map
                .get(&key)
                .with_context(|| format!("Error paring {local_file}: invalid reference {ref_path}"))?
                .clone();

            tracing::debug!("Resolving reference - SUCCESS");
            ref_resolve(&ref_file, &mut ref_val, ref_map)
        }
        _ => anyhow::Ok(val.clone()),
    }
}

#[tracing::instrument(skip(ref_map))]
fn ref_resolved(
    ref_map: &HashMap<String, (String, serde_json::Value)>,
) -> Result<serde_json::Map<String, serde_json::Value>, anyhow::Error> {
    tracing::info!("Resolving references");

    let acc = serde_json::Map::with_capacity(ref_map.len());

    ref_map.iter().try_fold(acc, |mut acc, (key, (local_file, val))| {
        let span = tracing::debug_span!("Resolving file", local_file, key).entered();
        acc.insert(key.clone(), ref_resolve(&local_file, val, &ref_map)?);
        span.exit();

        anyhow::Ok(acc)
    })
}

#[tracing::instrument]
fn ref_replace(
    local_file: &str,
    val: &serde_json::Value,
    deref_map: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<serde_json::Value> {
    match val {
        serde_json::Value::Object(object) => {
            let object =
                object.iter().try_fold(serde_json::Map::with_capacity(object.len()), |mut acc, (key, val)| {
                    if key == "$ref" && matches!(val, serde_json::Value::String(_)) {
                        acc.insert(key.clone(), ref_replace(local_file, val, deref_map)?);
                    } else {
                        acc.insert(key.clone(), val.clone());
                    }
                    anyhow::Ok(acc)
                })?;
            anyhow::Ok(serde_json::Value::Object(object))
        }
        serde_json::Value::String(string) => deref_map
            .get(string)
            .cloned()
            .with_context(|| format!("Error parsing {local_file}: reference '{string}' does not exists")),
        _ => anyhow::Ok(val.clone()),
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;

    type JsonInfo = (serde_json::Map<String, serde_json::Value>, String);

    #[rstest::fixture]
    #[once]
    fn component_a() -> serde_json::Value {
        serde_json::json!(
            {
                "foo": {
                    "$ref": "#components/schemas/COMPONENT_B"
                }
            }
        )
    }

    #[rstest::fixture]
    #[once]
    fn component_a_resolved(component_b: &serde_json::Value) -> serde_json::Value {
        serde_json::json!(
            {
                "foo": component_b
            }
        )
    }

    #[rstest::fixture]
    #[once]
    fn component_b() -> serde_json::Value {
        serde_json::json!({
           "bazz": {
                "type": "string"
            }
        })
    }

    #[rstest::fixture]
    #[once]
    fn component_c() -> serde_json::Value {
        serde_json::json!({
            "foo": {
                "$ref": "./file_a.json/#components/schemas/COMPONENT_A"
            }
        })
    }

    #[rstest::fixture]
    #[once]
    fn component_c_resolved(component_a_resolved: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "foo": component_a_resolved
        })
    }

    #[rstest::fixture]
    #[once]
    fn component_d() -> serde_json::Value {
        serde_json::json!({
            "bazz": {
                "type": "string",
                "pattern": "^Hello"
            }
        })
    }

    #[rstest::fixture]
    #[once]
    fn file_a(component_a: &serde_json::Value, component_b: &serde_json::Value) -> JsonInfo {
        (
            serde_json::json!(
                {
                    "components": {
                        "schemas": {
                            "COMPONENT_A": component_a,
                            "COMPONENT_B": component_b
                        }
                    }
                }
            )
            .as_object()
            .unwrap()
            .clone(),
            "./file_a.json".to_string(),
        )
    }

    #[rstest::fixture]
    #[once]
    fn file_b(component_c: &serde_json::Value, component_d: &serde_json::Value) -> JsonInfo {
        (
            serde_json::json!(
                {
                    "components": {
                        "schemas": {
                            "COMPONENT_C": component_c,
                            "COMPONENT_D": component_d
                        }
                    }
                }
            )
            .as_object()
            .unwrap()
            .clone(),
            "./file_b.json".to_string(),
        )
    }

    #[rstest::rstest]
    fn ref_load_valid(
        file_a: &JsonInfo,
        file_b: &JsonInfo,
        component_a: &serde_json::Value,
        component_b: &serde_json::Value,
        component_c: &serde_json::Value,
        component_d: &serde_json::Value,
    ) {
        crate::logger_init();

        let refs = crate::ref_load(&vec![file_a.clone(), file_b.clone()]).unwrap();
        let mut expected = HashMap::default();
        expected.insert("./file_a.jsonCOMPONENT_A".to_string(), ("./file_a.json".to_string(), component_a.clone()));
        expected.insert("./file_a.jsonCOMPONENT_B".to_string(), ("./file_a.json".to_string(), component_b.clone()));
        expected.insert("./file_b.jsonCOMPONENT_C".to_string(), ("./file_b.json".to_string(), component_c.clone()));
        expected.insert("./file_b.jsonCOMPONENT_D".to_string(), ("./file_b.json".to_string(), component_d.clone()));

        assert_eq!(
            refs,
            expected,
            "{} != {}",
            serde_json::to_string_pretty(&refs).unwrap(),
            serde_json::to_string_pretty(&expected).unwrap()
        );
    }

    #[rstest::rstest]
    // TODO: declare files here, with different component order permutations
    fn ref_resolve_valid(
        file_a: &JsonInfo,
        file_b: &JsonInfo,
        component_a_resolved: &serde_json::Value,
        component_b: &serde_json::Value,
        component_c_resolved: &serde_json::Value,
        component_d: &serde_json::Value,
    ) {
        crate::logger_init();

        let refs = crate::ref_load(&vec![file_a.clone(), file_b.clone()]).unwrap();
        tracing::trace!("Raw references are: {}", serde_json::to_string_pretty(&refs).unwrap());

        let refs_resolved = crate::ref_resolved(&refs).unwrap();
        let mut expected = serde_json::Map::default();
        expected.insert("./file_a.jsonCOMPONENT_A".to_string(), component_a_resolved.clone());
        expected.insert("./file_a.jsonCOMPONENT_B".to_string(), component_b.clone());
        expected.insert("./file_b.jsonCOMPONENT_C".to_string(), component_c_resolved.clone());
        expected.insert("./file_b.jsonCOMPONENT_D".to_string(), component_d.clone());

        assert_eq!(
            refs_resolved,
            expected,
            "{} != {}",
            serde_json::to_string_pretty(&refs_resolved).unwrap(),
            serde_json::to_string_pretty(&expected).unwrap()
        )
    }
}
