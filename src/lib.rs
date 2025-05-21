#![warn(clippy::all, clippy::pedantic)]
use once_cell::sync::OnceCell;
use pyo3::{
    ffi,
    prelude::*,
    types::{PyAny, PyDict},
};
use rusqlite::{params, Connection, OpenFlags};
use std::{
    cell::RefCell, ffi::CStr, os::raw::c_int, path::PathBuf, ptr, sync::Mutex, time::Duration,
};

static DB_CONN: OnceCell<Mutex<Connection>> = OnceCell::new();

static ROOT_PREFIX: OnceCell<String> = OnceCell::new();
static VENV_PREFIX: OnceCell<String> = OnceCell::new();

thread_local! {
    static TEST_NODEID: RefCell<Option<String>> = const { RefCell::new(None) };
    static TRACE_EVENTS: RefCell<Vec<(String, usize)>> = const { RefCell::new(Vec::new()) };
    static ENABLED: RefCell<bool> = const { RefCell::new(false) };
}

#[pyfunction]
fn pytest_addoption(py: Python, parser: &Bound<'_, PyAny>) -> PyResult<()> {
    let group = parser.call_method1("getgroup", ("skippy-tracer", "Options for skippy-tracer"))?;
    let builtins = py.import("builtins")?;
    let str_type = builtins.getattr("str")?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("required", false)?;
    kwargs.set_item("type", str_type)?;
    kwargs.set_item("default", py.None())?;
    group.call_method("addoption", ("--cov-db",), Some(&kwargs))?;
    Ok(())
}

#[pyfunction]
fn pytest_configure(py: Python, config: &Bound<'_, PyAny>) -> PyResult<()> {
    let db_path: Option<String> = config
        .call_method1("getoption", ("--cov-db", py.None()))?
        .extract()?;

    let db_path = if let Some(db_path) = db_path {
        ENABLED.with(|t| *t.borrow_mut() = true);
        db_path
    } else {
        return Ok(());
    };

    let root = config
        .getattr("rootpath")?
        .extract::<PathBuf>()?
        .canonicalize()
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?
        .to_string_lossy()
        .into_owned();
    let sys = config.py().import("sys")?;
    let venv = sys
        .getattr("prefix")?
        .extract::<PathBuf>()?
        .canonicalize()
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?
        .to_string_lossy()
        .into_owned();

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
    )
    .unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS trace (
             id      INTEGER PRIMARY KEY,
             nodeid  TEXT NOT NULL,
             file    TEXT NOT NULL,
             line    INTEGER NOT NULL
         );

         CREATE UNIQUE INDEX IF NOT EXISTS unique_line ON trace (nodeid, file, line);
",
    )
    .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

    DB_CONN.set(Mutex::new(conn)).ok();
    ROOT_PREFIX
        .set(root)
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
    VENV_PREFIX
        .set(venv)
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
    Ok(())
}

#[pyfunction]
#[allow(unused_variables)]
fn pytest_runtest_logstart(_py: Python, nodeid: &str, location: &Bound<'_, PyAny>) {
    if !ENABLED.with(|t| *t.borrow()) {
        return;
    }
    TEST_NODEID.with(|t| *t.borrow_mut() = Some(nodeid.to_string()));
    TRACE_EVENTS.with(|b| b.borrow_mut().clear());
    unsafe {
        ffi::PyEval_SetTrace(Some(trace_callback), ptr::null_mut());
    }
}

#[pyfunction]
#[allow(unused_variables)]
fn pytest_runtest_logfinish(_py: Python, nodeid: &str, location: &Bound<'_, PyAny>) {
    if !ENABLED.with(|t| *t.borrow()) {
        return;
    }
    unsafe {
        ffi::PyEval_SetTrace(None, ptr::null_mut());
    }
    let conn_mutex = DB_CONN.get().expect("DB not initialized");
    TRACE_EVENTS.with(|b| {
        if let Some(id) = TEST_NODEID.with(|t| t.borrow().clone()) {
            let conn = conn_mutex.lock().unwrap();

            for (file, line) in b.borrow().iter() {
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO trace (nodeid, file, line) VALUES (?1, ?2, ?3)",
                    params![id, file, *line],
                );
            }
        }
    });
    TEST_NODEID.with(|t| *t.borrow_mut() = None);
}

#[pymodule]
fn skippy_tracer(_py: Python, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(pytest_configure, m)?)?;
    m.add_function(wrap_pyfunction!(pytest_runtest_logstart, m)?)?;
    m.add_function(wrap_pyfunction!(pytest_runtest_logfinish, m)?)?;
    m.add_function(wrap_pyfunction!(pytest_addoption, m)?)?;
    Ok(())
}

extern "C" fn trace_callback(
    _obj: *mut ffi::PyObject,
    frame: *mut ffi::PyFrameObject,
    what: c_int,
    _arg: *mut ffi::PyObject,
) -> c_int {
    if what != ffi::PyTrace_LINE {
        return 0;
    }
    let code = {
        #[cfg(not(Py_3_9))]
        unsafe {
            (*frame).f_code
        }
        #[cfg(Py_3_9)]
        unsafe {
            ffi::PyFrame_GetCode(frame)
        }
    };
    let filename_ptr = unsafe { (*code).co_filename };
    if filename_ptr.is_null() {
        return 0;
    }
    let c_str = unsafe { ffi::PyUnicode_AsUTF8(filename_ptr) };
    if c_str.is_null() {
        return 0;
    }
    let bytes = unsafe { CStr::from_ptr(c_str).to_bytes() };
    let root = ROOT_PREFIX.get().expect("root not set");
    let venv = VENV_PREFIX.get().expect("venv not set");
    if bytes.starts_with(root.as_bytes()) && !bytes.starts_with(venv.as_bytes()) {
        #[allow(clippy::cast_sign_loss)] // line numbers are allways positive
        let lineno = unsafe { ffi::PyFrame_GetLineNumber(frame) as usize };
        let file = String::from_utf8_lossy(bytes).into_owned();
        TRACE_EVENTS.with(|b| b.borrow_mut().push((file, lineno)));
    }
    0
}
