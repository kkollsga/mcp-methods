//! Python extension layer.
//!
//! Loads manifest-declared `python:` tool hooks (and a custom embedder
//! factory, when configured) into the rmcp server. CPython is embedded
//! via PyO3's `auto-initialize` feature, so the binary doesn't need a
//! separate Python install on the operator's machine — only the user's
//! own Python tool/embedder source files on disk.
//!
//! Tool dispatch:
//! 1. Manifest entry: `tools: [{ name, python: ./tools.py, function: detail }]`
//! 2. At boot, the user file is loaded via `importlib.util` (no
//!    `sys.path` mutation) and the named function extracted.
//! 3. The function's `inspect.signature()` is mined for parameter types
//!    + defaults; this becomes the MCP tool's input JSON Schema.
//! 4. Each invocation acquires the GIL, builds a `**kwargs` PyDict from
//!    the JSON arguments, calls the function, and returns its string
//!    result (or `repr()` if the function returned a non-string).
//!
//! Trust gates:
//!   `python:` tools require `--trust-tools` + `trust.allow_python_tools: true`.
//!   Custom embedders require `--trust-tools` + `trust.allow_embedder: true`.
//!   Either signal alone refuses to load.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};
use rmcp::handler::server::router::tool::{ToolRoute, ToolRouter};
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{CallToolResult, Content, Tool};
use rmcp::ErrorData as McpError;

use crate::manifest::{EmbedderConfig, Manifest, PythonTool};
use crate::server::McpServer;

type DynFut<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// One-shot interpreter readiness check. PyO3's `auto-initialize` does
/// this lazily on first GIL acquisition; calling here turns surprises
/// into a clear startup error if the embedded interpreter is missing.
pub fn ensure_python() -> Result<()> {
    Python::attach(|py| -> PyResult<()> {
        let v: String = py.import("sys")?.getattr("version")?.extract()?;
        tracing::info!(python_version = %v.lines().next().unwrap_or(""), "embedded Python ready");
        Ok(())
    })
    .context("failed to initialize embedded Python interpreter")?;
    Ok(())
}

/// Discover `python:` tool callables from the manifest, build their
/// JSON Schemas, and register dynamic tool routes on the supplied
/// router. Returns the count registered.
pub fn register_python_tools(
    router: &mut ToolRouter<McpServer>,
    manifest: &Manifest,
) -> Result<usize> {
    let python_tools: Vec<&PythonTool> = manifest
        .tools
        .iter()
        .filter_map(|t| match t {
            crate::manifest::ToolSpec::Python(p) => Some(p),
            _ => None,
        })
        .collect();
    if python_tools.is_empty() {
        return Ok(0);
    }

    let manifest_dir = manifest
        .yaml_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut count = 0;
    for spec in python_tools {
        let route = build_python_tool_route(spec, &manifest_dir)?;
        router.add_route(route);
        count += 1;
    }
    Ok(count)
}

fn build_python_tool_route(spec: &PythonTool, manifest_dir: &Path) -> Result<ToolRoute<McpServer>> {
    let abs_path = manifest_dir
        .join(&spec.python)
        .canonicalize()
        .with_context(|| {
            format!(
                "python tool {:?}: file {:?} does not exist or could not be canonicalised",
                spec.name, spec.python
            )
        })?;

    let (callable, schema, description) = Python::attach(|py| -> PyResult<_> {
        let py_fn = load_python_function(py, &abs_path, &spec.function)?;
        let (sig_schema, sig_description) = introspect_signature(py, &py_fn)?;
        let final_schema = spec
            .parameters
            .clone()
            .map(json_value_to_object)
            .transpose()?
            .unwrap_or(sig_schema);
        let final_description = spec.description.clone().or(sig_description);
        Ok((py_fn, final_schema, final_description))
    })
    .with_context(|| format!("python tool {:?}: failed to load", spec.name))?;

    let tool_attr = build_tool_attr(&spec.name, description.as_deref(), schema);
    let py_fn: Arc<Py<PyAny>> = Arc::new(callable);
    let tool_name = spec.name.clone();

    Ok(ToolRoute::new_dyn(
        tool_attr,
        move |ctx: ToolCallContext<'_, McpServer>| -> DynFut<'_, Result<CallToolResult, McpError>> {
            let py_fn = py_fn.clone();
            let name = tool_name.clone();
            let arguments = ctx.arguments.clone();
            Box::pin(async move {
                let body = invoke_python_tool(&py_fn, arguments)
                    .unwrap_or_else(|e| format!("python tool {name:?} error: {e}"));
                Ok(CallToolResult::success(vec![Content::text(body)]))
            })
        },
    ))
}

fn invoke_python_tool(
    callable: &Arc<Py<PyAny>>,
    arguments: Option<rmcp::model::JsonObject>,
) -> Result<String> {
    Python::attach(|py| -> PyResult<String> {
        let kwargs = PyDict::new(py);
        if let Some(map) = arguments {
            for (k, v) in map {
                let py_val = json_to_py(py, &v)?;
                kwargs.set_item(k, py_val)?;
            }
        }
        let args = PyTuple::empty(py);
        let result = callable.call(py, args, Some(&kwargs))?;
        py_result_to_string(py, &result)
    })
    .map_err(|e| anyhow!(e.to_string()))
}

/// Convert a PyAny return value into a string suitable for tool output.
fn py_result_to_string(py: Python<'_>, val: &Py<PyAny>) -> PyResult<String> {
    if let Ok(s) = val.extract::<String>(py) {
        return Ok(s);
    }
    if let Ok(json_mod) = py.import("json") {
        if let Ok(out) = json_mod.call_method1("dumps", (val,)) {
            if let Ok(s) = out.extract::<String>() {
                return Ok(s);
            }
        }
    }
    let repr = val.bind(py).repr()?.to_string();
    Ok(repr)
}

/// Recursively convert serde_json values to Py<PyAny>s.
///
/// Public so downstream binaries can reuse the same conversion when
/// dispatching their own dynamic Python tools (e.g. kglite's
/// graph_overview which forwards JSON kwargs to `graph.describe`).
pub fn json_to_py(py: Python<'_>, val: &serde_json::Value) -> PyResult<Py<PyAny>> {
    use serde_json::Value as V;
    Ok(match val {
        V::Null => py.None(),
        V::Bool(b) => pyo3::types::PyBool::new(py, *b)
            .to_owned()
            .unbind()
            .into_any(),
        V::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_pyobject(py)?.unbind().into_any()
            } else if let Some(u) = n.as_u64() {
                u.into_pyobject(py)?.unbind().into_any()
            } else {
                n.as_f64()
                    .unwrap_or(0.0)
                    .into_pyobject(py)?
                    .unbind()
                    .into_any()
            }
        }
        V::String(s) => s.into_pyobject(py)?.unbind().into_any(),
        V::Array(arr) => {
            let list = PyList::empty(py);
            for item in arr {
                list.append(json_to_py(py, item)?)?;
            }
            list.unbind().into_any()
        }
        V::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, json_to_py(py, v)?)?;
            }
            dict.unbind().into_any()
        }
    })
}

/// Load a Python function from a .py file via importlib.util.
fn load_python_function(py: Python<'_>, path: &Path, function_name: &str) -> PyResult<Py<PyAny>> {
    let importlib_util = py.import("importlib.util")?;
    let module_name = format!("_mcp_user_tool_{function_name}");
    let path_str = path.to_string_lossy();
    let spec =
        importlib_util.call_method1("spec_from_file_location", (&module_name, &*path_str))?;
    if spec.is_none() {
        return Err(pyo3::exceptions::PyImportError::new_err(format!(
            "could not load module from {path_str}"
        )));
    }
    let module = importlib_util.call_method1("module_from_spec", (&spec,))?;
    let loader = spec.getattr("loader")?;
    loader.call_method1("exec_module", (&module,))?;
    let callable = module.getattr(function_name)?;
    if !callable.is_callable() {
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "'{function_name}' in {path_str} is not callable"
        )));
    }
    Ok(callable.unbind())
}

/// Walk a Python callable's signature to derive a JSON Schema for MCP.
fn introspect_signature(
    py: Python<'_>,
    callable: &Py<PyAny>,
) -> PyResult<(serde_json::Map<String, serde_json::Value>, Option<String>)> {
    let inspect = py.import("inspect")?;
    let sig = inspect.call_method1("signature", (callable,))?;
    let params = sig.getattr("parameters")?;
    let items = params.call_method0("items")?;

    let mut properties = serde_json::Map::new();
    let mut required: Vec<String> = Vec::new();

    for entry in items.try_iter()? {
        let entry = entry?;
        let tup = entry.cast::<PyTuple>()?;
        let name: String = tup.get_item(0)?.extract()?;
        let p = tup.get_item(1)?;

        let kind = p.getattr("kind")?;
        let kind_repr: String = kind.repr()?.to_string();
        if kind_repr.contains("VAR_POSITIONAL") || kind_repr.contains("VAR_KEYWORD") {
            continue;
        }

        let annotation = p.getattr("annotation")?;
        let annotation_obj = annotation.as_unbound().clone_ref(py);
        let mut prop_schema = annotation_to_schema(py, &annotation_obj)?;

        let default = p.getattr("default")?;
        let empty = inspect.getattr("Parameter")?.getattr("empty")?;
        let has_default = default.is(&empty);
        if has_default {
            required.push(name.clone());
        } else if !default.is_none() {
            let default_obj = default.as_unbound().clone_ref(py);
            if let Ok(json_default) = py_to_json(py, &default_obj) {
                prop_schema.insert("default".to_string(), json_default);
            }
        }

        properties.insert(name, serde_json::Value::Object(prop_schema));
    }

    let mut schema = serde_json::Map::new();
    schema.insert(
        "type".to_string(),
        serde_json::Value::String("object".to_string()),
    );
    schema.insert(
        "properties".to_string(),
        serde_json::Value::Object(properties),
    );
    if !required.is_empty() {
        schema.insert(
            "required".to_string(),
            serde_json::Value::Array(
                required
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }

    let docstring: Option<String> = callable
        .bind(py)
        .getattr("__doc__")
        .ok()
        .and_then(|d| d.extract().ok());

    Ok((schema, docstring))
}

/// Map a Python annotation (typing object or builtin type) to a JSON Schema fragment.
fn annotation_to_schema(
    py: Python<'_>,
    annotation: &Py<PyAny>,
) -> PyResult<serde_json::Map<String, serde_json::Value>> {
    let mut schema = serde_json::Map::new();

    let inspect = py.import("inspect")?;
    let empty = inspect.getattr("Parameter")?.getattr("empty")?;
    if annotation.bind(py).is(&empty) {
        schema.insert("type".to_string(), "string".into());
        return Ok(schema);
    }

    let builtins = py.import("builtins")?;
    let str_t = builtins.getattr("str")?;
    let int_t = builtins.getattr("int")?;
    let float_t = builtins.getattr("float")?;
    let bool_t = builtins.getattr("bool")?;
    let list_t = builtins.getattr("list")?;
    let dict_t = builtins.getattr("dict")?;

    let bound = annotation.bind(py);
    if bound.is(&str_t) {
        schema.insert("type".to_string(), "string".into());
    } else if bound.is(&bool_t) {
        // bool MUST be checked before int (bool is subclass of int in Python)
        schema.insert("type".to_string(), "boolean".into());
    } else if bound.is(&int_t) {
        schema.insert("type".to_string(), "integer".into());
    } else if bound.is(&float_t) {
        schema.insert("type".to_string(), "number".into());
    } else if bound.is(&list_t) {
        schema.insert("type".to_string(), "array".into());
    } else if bound.is(&dict_t) {
        schema.insert("type".to_string(), "object".into());
    } else {
        schema.insert("type".to_string(), "string".into());
        if let Ok(repr) = bound.repr() {
            schema.insert("title".to_string(), repr.to_string().into());
        }
    }
    Ok(schema)
}

/// Best-effort PyAny → serde_json::Value for default values.
fn py_to_json(py: Python<'_>, val: &Py<PyAny>) -> PyResult<serde_json::Value> {
    if val.is_none(py) {
        return Ok(serde_json::Value::Null);
    }
    if let Ok(b) = val.extract::<bool>(py) {
        return Ok(serde_json::Value::Bool(b));
    }
    if let Ok(i) = val.extract::<i64>(py) {
        return Ok(serde_json::Value::Number(i.into()));
    }
    if let Ok(f) = val.extract::<f64>(py) {
        return Ok(serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null));
    }
    if let Ok(s) = val.extract::<String>(py) {
        return Ok(serde_json::Value::String(s));
    }
    let json_mod = py.import("json")?;
    let dumped: String = json_mod.call_method1("dumps", (val,))?.extract()?;
    serde_json::from_str(&dumped).map_err(|e| {
        pyo3::exceptions::PyValueError::new_err(format!("default not JSON-serialisable: {e}"))
    })
}

fn json_value_to_object(
    val: serde_json::Value,
) -> PyResult<serde_json::Map<String, serde_json::Value>> {
    match val {
        serde_json::Value::Object(o) => Ok(o),
        other => Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "expected JSON object, got {other:?}"
        ))),
    }
}

/// Build an rmcp [`Tool`] attr from the user-facing pieces of a
/// dynamic tool registration: name, optional description, and an
/// arbitrary JSON Schema map.
///
/// Public so downstream consumers that build their own dynamic
/// `ToolRoute::new_dyn` registrations (without going through the
/// framework's `register_*` helpers) can construct the matching
/// attr in the same shape the framework uses.
pub fn build_tool_attr(
    name: &str,
    description: Option<&str>,
    schema: serde_json::Map<String, serde_json::Value>,
) -> Tool {
    Tool::new_with_raw(
        name.to_string(),
        description.map(|s| std::borrow::Cow::Owned(s.to_string())),
        Arc::new(schema),
    )
}

// ---------------------------------------------------------------------------
// Embedder factory loading
// ---------------------------------------------------------------------------

/// Load a manifest-declared embedder factory and return the
/// instantiated Py<PyAny>.
pub fn load_embedder(config: &EmbedderConfig, manifest_dir: &Path) -> Result<Py<PyAny>> {
    let abs_path = manifest_dir
        .join(&config.module)
        .canonicalize()
        .with_context(|| {
            format!(
                "embedder module {:?} does not exist or could not be canonicalised",
                config.module
            )
        })?;

    Python::attach(|py| -> PyResult<Py<PyAny>> {
        let module = load_python_module(py, &abs_path, "_mcp_user_embedder")?;
        let class = module.getattr(config.class.as_str())?;
        if !class.is_callable() {
            return Err(pyo3::exceptions::PyTypeError::new_err(format!(
                "embedder class {:?} is not callable",
                config.class
            )));
        }
        let kwargs = PyDict::new(py);
        for (k, v) in &config.kwargs {
            kwargs.set_item(k, json_to_py(py, v)?)?;
        }
        let instance = class.call((), Some(&kwargs))?;
        Ok(instance.unbind())
    })
    .with_context(|| format!("failed to instantiate embedder {:?}", config.class))
}

fn load_python_module<'py>(
    py: Python<'py>,
    path: &Path,
    module_name: &str,
) -> PyResult<Bound<'py, PyAny>> {
    let importlib_util = py.import("importlib.util")?;
    let path_str = path.to_string_lossy();
    let spec = importlib_util.call_method1("spec_from_file_location", (module_name, &*path_str))?;
    if spec.is_none() {
        return Err(pyo3::exceptions::PyImportError::new_err(format!(
            "could not load module from {path_str}"
        )));
    }
    let module = importlib_util.call_method1("module_from_spec", (&spec,))?;
    let loader = spec.getattr("loader")?;
    loader.call_method1("exec_module", (&module,))?;
    Ok(module)
}
