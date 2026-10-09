//! Synchronous GUI profile hooks in an isolated Rust JavaScript context.
//! No filesystem/module loader, network host, process host or job pump is installed.

use std::{collections::HashSet, path::Path, rc::Rc};

use anyhow::{Context as _, bail};
use boa_engine::{Context, JsValue, Source, js_string, module::IdleModuleLoader};
use serde_yaml_ng::Mapping;

const MAX_BYTES: usize = 10 * 1024 * 1024;
const MAX_VALUES: usize = 200_000;
const MAX_DEPTH: usize = 128;

pub fn evaluate(config: &Mapping, script: &str, profile_name: &str, path: &Path) -> anyhow::Result<Mapping> {
    let source = path.display();
    if script.len() > MAX_BYTES {
        bail!("profile script {source}: source exceeds 10 MiB limit");
    }
    // Match the GUI default hook shortcut and keep all YAML values exactly,
    // including unknown non-JSON keys and integers outside JavaScript's range.
    if script.trim() == clash_verge_core::utils::tmpl::ITEM_SCRIPT.trim() {
        return Ok(config.clone());
    }
    let json = serde_json::to_value(config).context("profile script input is not JSON-compatible")?;
    if serde_json::to_vec(&json)?.len() > MAX_BYTES {
        bail!("profile script {source}: input exceeds 10 MiB limit");
    }
    let mut remaining = MAX_VALUES;
    check_input(&json, 0, &mut remaining)?;
    let mut context = Context::builder()
        .module_loader(Rc::new(IdleModuleLoader))
        .can_block(false)
        .instructions_remaining(5_000_000)
        .build()
        .map_err(|error| anyhow::anyhow!("profile script {source}: engine initialization failed: {error}"))?;
    let limits = context.runtime_limits_mut();
    limits.set_loop_iteration_limit(1_000_000);
    limits.set_recursion_limit(128);
    limits.set_stack_size_limit(8_192);
    // GUI hooks commonly log. Keep those functions compatible without leaking
    // profile data into terminal output or providing any external host access.
    context
        .eval(Source::from_bytes(
            "const console = Object.freeze({log(){},info(){},warn(){},error(){},debug(){},table(){}});",
        ))
        .map_err(|error| anyhow::anyhow!("profile script {source}: console initialization failed: {error}"))?;
    let json_object = js(context.global_object().get(js_string!("JSON"), &mut context))?;
    let stringify = json_object
        .as_object()
        .context("JSON intrinsic missing")?
        .get(js_string!("stringify"), &mut context)
        .map_err(js_error)?
        .as_callable()
        .context("JSON intrinsic missing")?;
    context.eval(Source::from_bytes(script)).map_err(|error| {
        let category = if error.as_native().is_some_and(|error| error.is_syntax()) {
            "syntax error"
        } else {
            "runtime error"
        };
        anyhow::anyhow!("profile script {source}: {category}: {error}")
    })?;
    let main = context
        .eval(Source::from_bytes("typeof main === 'function' ? main : undefined"))
        .map_err(|error| anyhow::anyhow!("profile script {source}: entry error: {error}"))?
        .as_callable()
        .with_context(|| format!("profile script {source}: missing main(config, profileName) function"))?;
    // Pass values as engine objects, never interpolate configuration or names
    // into JavaScript source.
    let input = js(JsValue::from_json(&json, &mut context))?;
    let name = boa_engine::JsString::from(profile_name);
    let result = main
        .call(&JsValue::undefined(), &[input, name.into()], &mut context)
        .map_err(|error| anyhow::anyhow!("profile script {source}: runtime error or execution limit: {error}"))?;
    if result.is_undefined() {
        bail!("profile script {source}: main returned undefined; return the configuration object");
    }
    if result.is_promise() {
        bail!("profile script {source}: async/Promise results are unsupported; main must return synchronously");
    }
    let object = result
        .as_object()
        .filter(|object| !object.is_array())
        .with_context(|| format!("profile script {source}: main must return a configuration object"))?;
    let then = object
        .get(js_string!("then"), &mut context)
        .map_err(|error| anyhow::anyhow!("profile script {source}: invalid result: {error}"))?;
    if then.is_callable() {
        bail!("profile script {source}: async/thenable results are unsupported");
    }
    let mut remaining = MAX_VALUES;
    check_output(&result, &mut context, 0, &mut remaining, &mut HashSet::new())
        .with_context(|| format!("profile script {source}: invalid or oversized result"))?;
    let encoded = stringify
        .call(&json_object, &[result], &mut context)
        .map_err(|error| anyhow::anyhow!("profile script {source}: invalid result or execution limit: {error}"))?;
    let encoded = encoded
        .as_string()
        .with_context(|| format!("profile script {source}: result is not JSON-compatible"))?
        .to_std_string_escaped();
    if encoded.len() > MAX_BYTES {
        bail!("profile script {source}: output exceeds 10 MiB limit");
    }
    let json: serde_json::Value =
        serde_json::from_str(&encoded).with_context(|| format!("profile script {source}: invalid JSON result"))?;
    if !json.is_object() {
        bail!("profile script {source}: serialized result must be a configuration object");
    }
    let mapping: Mapping = serde_yaml_ng::from_str(&encoded)
        .with_context(|| format!("profile script {source}: invalid configuration result"))?;
    Ok(mapping
        .into_iter()
        .map(|(key, value)| {
            let key = key
                .as_str()
                .map(|key| serde_yaml_ng::Value::from(key.to_ascii_lowercase()))
                .unwrap_or(key);
            (key, value)
        })
        .collect())
}

fn js_error(error: boa_engine::JsError) -> anyhow::Error {
    anyhow::anyhow!("{error}")
}
fn js<T>(result: boa_engine::JsResult<T>) -> anyhow::Result<T> {
    result.map_err(js_error)
}

fn check_input(value: &serde_json::Value, depth: usize, remaining: &mut usize) -> anyhow::Result<()> {
    if depth > MAX_DEPTH || *remaining == 0 {
        bail!("profile script input exceeds structural limit");
    }
    *remaining -= 1;
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                check_input(value, depth + 1, remaining)?;
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                check_input(value, depth + 1, remaining)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_output(
    value: &JsValue,
    context: &mut Context,
    depth: usize,
    remaining: &mut usize,
    seen: &mut HashSet<boa_engine::object::JsObject>,
) -> anyhow::Result<()> {
    if depth > MAX_DEPTH || *remaining == 0 {
        bail!("result exceeds structural limit");
    }
    *remaining -= 1;
    if let Some(object) = value.as_object() {
        if !seen.insert(object.clone()) {
            bail!("cyclic configuration object");
        }
        if object.is_array() {
            let length = js(js(object.get(js_string!("length"), context))?.to_number(context))?;
            if length > MAX_VALUES as f64 {
                bail!("result array exceeds structural limit");
            }
        }
        let keys = js(object.own_property_keys(context))?;
        if keys.len() > *remaining {
            bail!("result exceeds structural limit");
        }
        for key in keys {
            check_output(&js(object.get(key, context))?, context, depth + 1, remaining, seen)?;
        }
        seen.remove(&object);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_errors_and_execution_bounds_leave_input_unchanged() {
        let config: Mapping = serde_yaml_ng::from_str("future: {keep: true}\nrules: [MATCH,DIRECT]\n").unwrap();
        let before = config.clone();
        for (script, expected) in [
            ("function main( {", "syntax error"),
            ("function main(c) { throw new Error('fixture'); }", "runtime error"),
            ("function main(c) {}", "undefined"),
            ("function main(c) { return null; }", "configuration object"),
            ("function main(c) { return []; }", "configuration object"),
            ("async function main(c) { return c; }", "async/Promise"),
            ("function main(c) { while (true) {} }", "execution limit"),
            ("function main(c) { return main(c); }", "execution limit"),
            ("function main(c) { c.self = c; return c; }", "cyclic"),
            (
                "function main(c) { c.huge = Array(4294967295); return c; }",
                "structural limit",
            ),
            (
                "function main(c) { fetch('https://fixture.invalid'); return c; }",
                "runtime error",
            ),
            ("function main(c) { require('fs'); return c; }", "runtime error"),
        ] {
            let error = format!(
                "{:#}",
                evaluate(&config, script, "fixture", Path::new("fixture.js")).unwrap_err()
            );
            assert!(error.contains(expected), "{expected}: {error}");
            assert_eq!(config, before);
        }
    }

    #[test]
    fn script_values_are_not_source_interpolated_and_contexts_are_isolated() {
        let config: Mapping = serde_yaml_ng::from_str("future: {keep: true}\n").unwrap();
        let name = "'); throw new Error('injection'); //";
        let result = evaluate(&config, "function main(c, name) { console.log(name); c.name = name; c.hosts = [typeof fetch, typeof require, typeof process]; return c; }", name, Path::new("fixture.js")).unwrap();
        assert_eq!(result["name"], serde_yaml_ng::Value::from(name));
        assert_eq!(result["hosts"][0], serde_yaml_ng::Value::from("undefined"));
        assert_eq!(result["hosts"][1], serde_yaml_ng::Value::from("undefined"));
        assert_eq!(result["hosts"][2], serde_yaml_ng::Value::from("undefined"));
        assert!(evaluate(&config, "main(config)", "fixture", Path::new("other.js")).is_err());
        let result = evaluate(
            &config,
            "function main(c) { c.FUTURE = { NestedKey: true }; return c; }",
            "fixture",
            Path::new("case.js"),
        )
        .unwrap();
        assert_eq!(result["future"]["NestedKey"], serde_yaml_ng::Value::from(true));
    }

    #[test]
    fn default_template_preserves_unknown_yaml_and_exact_large_integers() {
        let config: Mapping =
            serde_yaml_ng::from_str("future: {integer: 18446744073709551615}\n? [future, key]\n: retained\n").unwrap();
        let result = evaluate(
            &config,
            clash_verge_core::utils::tmpl::ITEM_SCRIPT,
            "fixture",
            Path::new("default.js"),
        )
        .unwrap();
        assert_eq!(result, config);
    }
}
