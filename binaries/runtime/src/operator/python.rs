#![allow(clippy::borrow_deref_ref)] // clippy warns about code generated by #[pymethods]

use super::{OperatorEvent, Tracer};
use dora_node_api::{communication::Publisher, config::DataId};
use dora_operator_api_python::metadata_to_pydict;
use eyre::{bail, eyre, Context};
use opentelemetry::trace::TraceContextExt;
use pyo3::{
    pyclass,
    types::IntoPyDict,
    types::{PyBytes, PyDict},
    Py, Python,
};
use std::{
    borrow::Cow,
    collections::HashMap,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::Arc,
    thread,
};
use tokio::sync::mpsc::Sender;

fn traceback(err: pyo3::PyErr) -> eyre::Report {
    Python::with_gil(|py| {
        eyre::Report::msg(format!(
            "{}\n{err}",
            err.traceback(py)
                .expect("PyError should have a traceback")
                .format()
                .expect("Traceback could not be formatted")
        ))
    })
}

pub fn spawn(
    path: &Path,
    events_tx: Sender<OperatorEvent>,
    inputs: flume::Receiver<dora_node_api::Input>,
    publishers: HashMap<DataId, Box<dyn Publisher>>,
    tracer: Tracer,
) -> eyre::Result<()> {
    if !path.exists() {
        bail!("No python file exists at {}", path.display());
    }
    let path = path
        .canonicalize()
        .wrap_err_with(|| format!("no file found at `{}`", path.display()))?;
    let path_cloned = path.clone();

    let send_output = SendOutputCallback {
        publishers: Arc::new(publishers),
    };

    let init_operator = move |py: Python| {
        if let Some(parent_path) = path.parent() {
            let parent_path = parent_path
                .to_str()
                .ok_or_else(|| eyre!("module path is not valid utf8"))?;
            let sys = py.import("sys").wrap_err("failed to import `sys` module")?;
            let sys_path = sys
                .getattr("path")
                .wrap_err("failed to import `sys.path` module")?;
            let sys_path_append = sys_path
                .getattr("append")
                .wrap_err("`sys.path.append` was not found")?;
            sys_path_append
                .call1((parent_path,))
                .wrap_err("failed to append module path to python search path")?;
        }

        let module_name = path
            .file_stem()
            .ok_or_else(|| eyre!("module path has no file stem"))?
            .to_str()
            .ok_or_else(|| eyre!("module file stem is not valid utf8"))?;
        let module = py.import(module_name).map_err(traceback)?;
        let operator_class = module
            .getattr("Operator")
            .wrap_err("no `Operator` class found in module")?;

        let locals = [("Operator", operator_class)].into_py_dict(py);
        let operator = py
            .eval("Operator()", None, Some(locals))
            .map_err(traceback)?;
        Result::<_, eyre::Report>::Ok(Py::from(operator))
    };

    let python_runner = move || {
        let operator =
            Python::with_gil(init_operator).wrap_err("failed to init python operator")?;

        while let Ok(mut input) = inputs.recv() {
            #[cfg(feature = "tracing")]
            let (_child_cx, string_cx) = {
                use dora_tracing::{deserialize_context, serialize_context};
                use opentelemetry::{trace::Tracer, Context as OtelContext};
                let cx = deserialize_context(&input.metadata.open_telemetry_context.to_string());
                let span = tracer.start_with_context(format!("{}", input.id), &cx);

                let child_cx = OtelContext::current_with_span(span);
                let string_cx = serialize_context(&child_cx);
                (child_cx, string_cx)
            };

            #[cfg(not(feature = "tracing"))]
            let string_cx = {
                let () = tracer;
                "".to_string()
            };
            input.metadata.open_telemetry_context = Cow::Owned(string_cx);

            let status_enum = Python::with_gil(|py| {
                let input_dict = PyDict::new(py);

                input_dict.set_item("id", input.id.as_str())?;
                input_dict.set_item("data", PyBytes::new(py, &input.data()))?;
                input_dict.set_item("metadata", metadata_to_pydict(input.metadata(), py))?;

                operator
                    .call_method1(py, "on_input", (input_dict, send_output.clone()))
                    .map_err(traceback)
            })?;
            let status_val = Python::with_gil(|py| status_enum.getattr(py, "value"))
                .wrap_err("on_input must have enum return value")?;
            let status: i32 = Python::with_gil(|py| status_val.extract(py))
                .wrap_err("on_input has invalid return value")?;
            match status {
                0 => {}     // ok
                1 => break, // stop
                other => bail!("on_input returned invalid status {other}"),
            }
        }

        Python::with_gil(|py| {
            let operator = operator.as_ref(py);
            if operator
                .hasattr("drop_operator")
                .wrap_err("failed to look for drop_operator")?
            {
                operator.call_method0("drop_operator")?;
            }
            Result::<_, eyre::Report>::Ok(())
        })?;

        Result::<_, eyre::Report>::Ok(())
    };

    thread::spawn(move || {
        let closure = AssertUnwindSafe(|| {
            python_runner()
                .wrap_err_with(|| format!("error in Python module at {}", path_cloned.display()))
        });

        match catch_unwind(closure) {
            Ok(Ok(())) => {
                let _ = events_tx.blocking_send(OperatorEvent::Finished);
            }
            Ok(Err(err)) => {
                let _ = events_tx.blocking_send(OperatorEvent::Error(err));
            }
            Err(panic) => {
                let _ = events_tx.blocking_send(OperatorEvent::Panic(panic));
            }
        }
    });

    Ok(())
}

#[pyclass]
#[derive(Clone)]
struct SendOutputCallback {
    publishers: Arc<HashMap<DataId, Box<dyn Publisher>>>,
}

#[allow(unsafe_op_in_unsafe_fn)]
mod callback_impl {

    use super::SendOutputCallback;
    use dora_operator_api_python::pydict_to_metadata;
    use eyre::{eyre, Context};
    use pyo3::{
        pymethods,
        types::{PyBytes, PyDict},
        PyResult,
    };

    #[pymethods]
    impl SendOutputCallback {
        fn __call__(
            &mut self,
            output: &str,
            data: &PyBytes,
            metadata: Option<&PyDict>,
        ) -> PyResult<()> {
            match self.publishers.get(output) {
                Some(publisher) => {
                    let message = pydict_to_metadata(metadata)?
                        .serialize()
                        .context(format!("failed to serialize `{}` metadata", output));
                    message.and_then(|mut message| {
                        message.extend_from_slice(data.as_bytes());
                        publisher
                            .publish(&message)
                            .map_err(|err| eyre::eyre!(err))
                            .context("publish failed")
                    })
                }
                None => Err(eyre!(
                    "unexpected output {output} (not defined in dataflow config)"
                )),
            }
            .map_err(|err| err.into())
        }
    }
}
