//! Python bindings for signal-ha.
//!
//! Exposes `HaClient`, `Scheduler`, and the `HaApp` base class to Python
//! via PyO3. Each automation runs in its own asyncio event loop backed by
//! a tokio runtime.

use pyo3::prelude::*;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

// Re-import from the signal-ha library crate (Cargo name: signal-ha → signal_ha)
use signal_ha as sha;

/// Create a shared tokio runtime for the module.
fn get_runtime() -> &'static Runtime {
    use std::sync::OnceLock;
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime")
    })
}

/// Convert a signal_ha client error into a Python exception.
fn to_py_err(e: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(format!("{e}"))
}

// ─── HaClient ──────────────────────────────────────────────────────

/// WebSocket client for Home Assistant.
///
/// Usage:
///     client = HaClient.connect("ws://ha:8123/api/websocket", "token")
///     state = client.get_state("sensor.porch_lux")
#[pyclass]
#[derive(Clone)]
struct HaClient {
    inner: Arc<Mutex<sha::HaClient>>,
}

#[pymethods]
impl HaClient {
    /// Connect and authenticate to Home Assistant.
    #[staticmethod]
    fn connect(url: &str, token: &str) -> PyResult<Self> {
        let rt = get_runtime();
        let client = rt
            .block_on(sha::HaClient::connect(url, token))
            .map_err(to_py_err)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(client)),
        })
    }

    /// Get the current state of an entity.
    fn get_state(&self, entity_id: &str) -> PyResult<EntityState> {
        let rt = get_runtime();
        let inner = self.inner.clone();
        let entity_id = entity_id.to_string();
        let state: sha::EntityState = rt
            .block_on(async {
                let client: tokio::sync::MutexGuard<'_, sha::HaClient> = inner.lock().await;
                client.get_state(&entity_id).await
            })
            .map_err(to_py_err)?;
        Ok(EntityState { inner: state })
    }

    /// Call a Home Assistant service.
    ///
    /// Args:
    ///     domain: Service domain (e.g. "light")
    ///     service: Service name (e.g. "turn_on")
    ///     data: Service data as a dict
    fn call_service(
        &self,
        domain: &str,
        service: &str,
        data: Option<&Bound<'_, pyo3::types::PyDict>>,
    ) -> PyResult<()> {
        let rt = get_runtime();
        let inner = self.inner.clone();
        let domain = domain.to_string();
        let service = service.to_string();

        // Convert Python dict to serde_json::Value
        let json_data: serde_json::Value = match data {
            Some(d) => {
                let py_str = d.str()?.to_string();
                serde_json::from_str(&py_str).unwrap_or(serde_json::Value::Object(
                    serde_json::Map::new(),
                ))
            }
            None => serde_json::Value::Object(serde_json::Map::new()),
        };

        rt.block_on(async {
            let client: tokio::sync::MutexGuard<'_, sha::HaClient> = inner.lock().await;
            client.call_service(&domain, &service, json_data).await
        })
        .map_err(to_py_err)
    }

    /// Send an arbitrary WebSocket message.
    fn send_raw(&self, msg: &str) -> PyResult<String> {
        let rt = get_runtime();
        let inner = self.inner.clone();
        let value: serde_json::Value =
            serde_json::from_str(msg).map_err(|e| PyValueError::new_err(e.to_string()))?;
        let result: serde_json::Value = rt
            .block_on(async {
                let client: tokio::sync::MutexGuard<'_, sha::HaClient> = inner.lock().await;
                client.send_raw(value).await
            })
            .map_err(to_py_err)?;
        Ok(result.to_string())
    }
}

// ─── EntityState ───────────────────────────────────────────────────

/// The state of a Home Assistant entity.
#[pyclass]
struct EntityState {
    inner: sha::EntityState,
}

#[pymethods]
impl EntityState {
    /// The entity's state value as a string.
    #[getter]
    fn state(&self) -> &str {
        &self.inner.state
    }

    /// The attributes as a JSON string.
    #[getter]
    fn attributes_json(&self) -> String {
        self.inner.attributes.to_string()
    }

    /// When the state last changed (ISO 8601).
    #[getter]
    fn last_changed(&self) -> String {
        self.inner.last_changed.to_rfc3339()
    }

    fn __repr__(&self) -> String {
        format!(
            "EntityState(state='{}', last_changed='{}')",
            self.inner.state,
            self.inner.last_changed.to_rfc3339()
        )
    }
}

// ─── Scheduler ─────────────────────────────────────────────────────

/// Sun-aware scheduler for a fixed geographic location.
#[pyclass]
struct Scheduler {
    inner: sha::Scheduler,
}

#[pymethods]
impl Scheduler {
    #[new]
    fn new(latitude: f64, longitude: f64) -> Self {
        Self {
            inner: sha::Scheduler::new(latitude, longitude),
        }
    }

    /// Whether the sun is currently up.
    fn is_sun_up(&self) -> bool {
        self.inner.is_sun_up()
    }

    /// Next sunrise as ISO 8601 string.
    fn next_sunrise(&self) -> String {
        self.inner.next_sunrise().to_rfc3339()
    }

    /// Next sunset as ISO 8601 string.
    fn next_sunset(&self) -> String {
        self.inner.next_sunset().to_rfc3339()
    }
}

// ─── HaApp base class ──────────────────────────────────────────────

/// Base class for porting AppDaemon apps to signal-ha.
///
/// Subclass this and implement `initialize()`. The class provides
/// AppDaemon-compatible helper methods.
///
/// Usage:
///     class PorchLights(HaApp):
///         def initialize(self):
///             self.listen_state(self.on_lux, "sensor.porch_lux")
#[pyclass(subclass)]
struct HaApp {
    client: Option<HaClient>,
    scheduler: Option<Scheduler>,
    #[pyo3(get)]
    args: PyObject,
}

#[pymethods]
impl HaApp {
    #[new]
    fn new(py: Python, args: Option<PyObject>) -> Self {
        Self {
            client: None,
            scheduler: None,
            args: args.unwrap_or_else(|| py.None()),
        }
    }

    /// Connect to HA and set up scheduler.
    fn run(&mut self, _py: Python, url: &str, token: &str, lat: f64, lon: f64) -> PyResult<()> {
        let client = HaClient::connect(url, token)?;
        let scheduler = Scheduler::new(lat, lon);
        self.client = Some(client);
        self.scheduler = Some(scheduler);
        Ok(())
    }

    /// Get the current state of an entity.
    ///
    /// Compatible with AppDaemon's get_state().
    fn get_state(&self, entity_id: &str, attribute: Option<&str>) -> PyResult<String> {
        let client = self.client.as_ref().ok_or_else(|| {
            PyRuntimeError::new_err("Not connected — call run() first")
        })?;
        let state = client.get_state(entity_id)?;
        match attribute {
            None => Ok(state.inner.state.clone()),
            Some(attr) => {
                let val = &state.inner.attributes[attr];
                Ok(val.to_string())
            }
        }
    }

    /// Call a Home Assistant service.
    ///
    /// Uses AppDaemon-style "domain/service" string.
    fn call_service(&self, service_str: &str, data: Option<&Bound<'_, pyo3::types::PyDict>>) -> PyResult<()> {
        let client = self.client.as_ref().ok_or_else(|| {
            PyRuntimeError::new_err("Not connected — call run() first")
        })?;
        let parts: Vec<&str> = service_str.splitn(2, '/').collect();
        if parts.len() != 2 {
            return Err(PyValueError::new_err(
                "service_str must be 'domain/service' format",
            ));
        }
        client.call_service(parts[0], parts[1], data)
    }

    /// Whether the sun is currently up.
    fn sun_up(&self) -> PyResult<bool> {
        let sched = self.scheduler.as_ref().ok_or_else(|| {
            PyRuntimeError::new_err("Not connected — call run() first")
        })?;
        Ok(sched.is_sun_up())
    }

    /// Log a message. Maps to Python's print for now.
    fn log(&self, message: &str, level: Option<&str>) {
        let lvl = level.unwrap_or("INFO");
        eprintln!("[{lvl}] {message}");
    }
}

// ─── Module ────────────────────────────────────────────────────────

/// signal_ha — Python bindings for the signal-ha Home Assistant automation library.
#[pymodule]
fn signal_ha_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<HaClient>()?;
    m.add_class::<EntityState>()?;
    m.add_class::<Scheduler>()?;
    m.add_class::<HaApp>()?;
    Ok(())
}
