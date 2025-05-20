use once_cell::sync::OnceCell;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyAny;
use rusqlite::{params, Connection, OpenFlags};
use std::cell::RefCell;
use std::ffi::CStr;
use std::os::raw::c_int;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::sync::Mutex;
use std::time::Duration;

static DB_CONN: OnceCell<Mutex<Connection>> = OnceCell::new();

static ROOT_PREFIX: OnceCell<String> = OnceCell::new();
static VENV_PREFIX: OnceCell<String> = OnceCell::new();

thread_local! {
    static TEST_NODEID: RefCell<Option<String>> = const { RefCell::new(None) };
    static TRACE_EVENTS: RefCell<Vec<(String, usize)>> = const { RefCell::new(Vec::new()) };
}

#[pyfunction]
fn pytest_configure(_py: Python, config: &Bound<'_, PyAny>) -> PyResult<()> {
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
        "skippy_trace.db",
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
         );",
    )
    .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

    DB_CONN.set(Mutex::new(conn)).ok();
    ROOT_PREFIX.set(root).ok();
    VENV_PREFIX.set(venv).ok();
    Ok(())
}

#[pyfunction]
fn pytest_runtest_call(_py: Python, item: &Bound<'_, PyAny>) -> PyResult<()> {
    let nodeid = item.getattr("nodeid")?.extract::<String>()?;
    TEST_NODEID.with(|t| *t.borrow_mut() = Some(nodeid.clone()));
    TRACE_EVENTS.with(|b| b.borrow_mut().clear());
    unsafe {
        ffi::PyEval_SetTrace(Some(trace_callback), ptr::null_mut());
    }
    let result = catch_unwind(AssertUnwindSafe(|| item.call_method0("runtest")));
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
    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => panic!("test panicked"),
    }
}

#[pymodule]
fn skippy_tracer(_py: Python, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(pytest_configure, m)?)?;
    m.add_function(wrap_pyfunction!(pytest_runtest_call, m)?)?;
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
        #[cfg(Py_3_8)]
        {
            unsafe { (*frame).f_code }
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
        let lineno = unsafe { ffi::PyFrame_GetLineNumber(frame) as usize };
        let file = String::from_utf8_lossy(bytes).into_owned();
        TRACE_EVENTS.with(|b| b.borrow_mut().push((file, lineno)));
    }
    0
}
