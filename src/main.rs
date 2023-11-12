use fs_err as fs;
use libc::wchar_t;
use libloading::Library;
use monotrail_utils::parse_cpython_args::{determine_python_version, naive_python_arg_parser};
use monotrail_utils::standalone_python::provision_python;
use ruff_python_formatter::{format_module_source, FormatModuleError, PyFormatOptions};
use std::error::Error;
use std::ffi::{c_int, c_void};
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::{env, io};
use tempfile::NamedTempFile;
use thiserror::Error;
use tracing::{debug, trace};
use widestring::WideCString;

#[derive(Debug, Error)]
enum PythonPlusPlusError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Failed to load symbol from libpython (libpython should contain those symbols)")]
    LibLoading(#[from] libloading::Error),
    #[error("Path contains non-utf8 characters: {0}")]
    InvalidPath(PathBuf),
    #[error(
        "Can't launch python, {0} does not exist even though it should have been just created"
    )]
    NoSuchExecutable(String),
    #[error("Failed to provision python")]
    ProvisionPython(#[source] anyhow::Error),
    #[error("Failed to determine python version")]
    DeterminePythonVersion(#[source] anyhow::Error),
    #[error("Failed to parse cpython arguments: {0}")]
    CpythonArgs(String),
    #[error("You need to pass a python script for this to work")]
    MissingScript,
    #[error("Invalid python code")]
    FormatModule(#[from] FormatModuleError),
}

/// <https://docs.python.org/3/c-api/init_config.html#preinitialize-python-with-pypreconfig>
///
/// <https://docs.rs/pyo3/0.16.5/pyo3/ffi/struct.PyPreConfig.html>
#[repr(C)]
#[derive(Debug)]
pub struct PyPreConfig {
    pub _config_init: c_int,
    pub parse_argv: c_int,
    pub isolated: c_int,
    pub use_environment: c_int,
    pub configure_locale: c_int,
    pub coerce_c_locale: c_int,
    pub coerce_c_locale_warn: c_int,
    #[cfg(windows)]
    pub legacy_windows_fs_encoding: c_int,
    pub utf8_mode: c_int,
    pub dev_mode: c_int,
    pub allocator: c_int,
}

/// <https://docs.rs/pyo3/0.16.5/pyo3/ffi/enum._PyStatus_TYPE.html>
#[repr(C)]
#[derive(Copy, Clone, Debug)]
#[allow(non_camel_case_types, clippy::enum_variant_names)]
pub enum _PyStatus_TYPE {
    _PyStatus_TYPE_OK,
    _PyStatus_TYPE_ERROR,
    _PyStatus_TYPE_EXIT,
}

/// <https://docs.python.org/3/c-api/init_config.html#pystatus>
///
/// <https://docs.rs/pyo3/0.16.5/pyo3/ffi/struct.PyStatus.html>
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct PyStatus {
    pub _type: _PyStatus_TYPE,
    pub func: *const i8,
    pub err_msg: *const i8,
    pub exitcode: c_int,
}

/// Set utf-8 mode through pre-init
///
/// <https://docs.python.org/3/c-api/init_config.html#preinitialize-python-with-pypreconfig>
unsafe fn pre_init(lib: &Library) -> Result<(), PythonPlusPlusError> {
    trace!("libpython pre-init");
    let py_pre_config_init_python_config: libloading::Symbol<
        unsafe extern "C" fn(*mut PyPreConfig) -> c_void,
    > = lib.get(b"PyPreConfig_InitPythonConfig")?;
    // It's all pretty much the c example code translated to rust
    let mut preconfig: MaybeUninit<PyPreConfig> = MaybeUninit::uninit();
    py_pre_config_init_python_config(preconfig.as_mut_ptr());
    let mut preconfig = preconfig.assume_init();
    // same as PYTHONUTF8=1
    preconfig.utf8_mode = 1;
    trace!("preconfig: {:?}", preconfig);

    let py_pre_initialize: libloading::Symbol<unsafe extern "C" fn(*mut PyPreConfig) -> PyStatus> =
        lib.get(b"Py_PreInitialize")?;
    let py_status_exception: libloading::Symbol<unsafe extern "C" fn(PyStatus) -> c_int> =
        lib.get(b"PyStatus_Exception")?;
    let py_exit_status_exception: libloading::Symbol<unsafe extern "C" fn(PyStatus) -> !> =
        lib.get(b"Py_ExitStatusException")?;

    // This is again from the example
    let status = py_pre_initialize(&mut preconfig as *mut PyPreConfig);
    #[allow(unreachable_code)]
    if py_status_exception(status) != 0 {
        debug!("libpython initialization error: {:?}", status);
        // This should never error, but who knows
        py_exit_status_exception(status);
        // I don't trust cpython
        #[allow(unreachable_code)]
        {
            unreachable!();
        }
    }
    Ok(())
}

/// The way we're using to load symbol by symbol with the type generic is really ugly and cumbersome
/// If you know how to do this with `extern` or even pyo3-ffi directly please tell me.
///
/// sys_executable is the monotrail runner since otherwise we don't get dependencies in
/// subprocesses.
///
/// Returns the exit code from python
fn inject_and_run_python(
    python_home: &Path,
    python_version: (u8, u8),
    sys_executable: &Path,
    args: &[String],
) -> Result<c_int, PythonPlusPlusError> {
    trace!(
        "Loading libpython {}.{}",
        python_version.0,
        python_version.1
    );

    let libpython3 = if cfg!(target_os = "windows") {
        // python3.dll doesn't include functions from the limited abi apparently
        python_home.join(format!("python3{}.dll", python_version.1))
    } else if cfg!(target_os = "macos") {
        python_home.join("lib").join(format!(
            "libpython{}.{}.dylib",
            python_version.0, python_version.1
        ))
    } else {
        python_home.join("lib").join("libpython3.so")
    };
    let lib = {
        // platform switch because we need to set RTLD_GLOBAL so extension modules work later
        #[cfg(unix)]
        {
            let flags = libloading::os::unix::RTLD_LAZY | libloading::os::unix::RTLD_GLOBAL;
            let unix_lib = unsafe { libloading::os::unix::Library::open(Some(libpython3), flags)? };
            libloading::Library::from(unix_lib)
        }
        // Entirely untested, but it should at least compile
        #[cfg(windows)]
        unsafe {
            let windows_lib = libloading::os::windows::Library::new(libpython3)?;
            libloading::Library::from(windows_lib)
        }
    };
    trace!("Initializing libpython");
    unsafe {
        // initialize python
        // TODO: Do this via python c api instead
        env::set_var("PYTHONNOUSERSITE", "1");

        pre_init(&lib)?;

        trace!("Py_SetPythonHome {}", python_home.display());
        // https://docs.python.org/3/c-api/init.html#c.Py_SetPythonHome
        // void Py_SetPythonHome(const wchar_t *name)
        // Otherwise we get an error that it can't find encoding that tells us to set PYTHONHOME
        let set_python_home: libloading::Symbol<unsafe extern "C" fn(*const wchar_t) -> c_void> =
            lib.get(b"Py_SetPythonHome")?;
        let python_home_wchar_t = WideCString::from_str(python_home.to_string_lossy()).unwrap();
        set_python_home(python_home_wchar_t.as_ptr() as *const wchar_t);

        let sys_executable_str = sys_executable
            .to_str()
            .ok_or_else(|| PythonPlusPlusError::InvalidPath(sys_executable.to_path_buf()))?;
        if !sys_executable.is_file() {
            return Err(PythonPlusPlusError::NoSuchExecutable(
                sys_executable_str.to_string(),
            ));
        }

        trace!("Py_SetProgramName {}", sys_executable_str);
        // https://docs.python.org/3/c-api/init.html#c.Py_SetProgramName
        // void Py_SetProgramName(const wchar_t *name)
        // To set sys.executable
        let set_program_name: libloading::Symbol<unsafe extern "C" fn(*const wchar_t) -> c_void> =
            lib.get(b"Py_SetProgramName")?;
        let sys_executable = WideCString::from_str(sys_executable_str).unwrap();
        set_program_name(sys_executable.as_ptr() as *const wchar_t);

        trace!("Py_Initialize");
        // https://docs.python.org/3/c-api/init.html?highlight=py_initialize#c.Py_Initialize
        // void Py_Initialize()
        let initialize: libloading::Symbol<unsafe extern "C" fn() -> c_void> =
            lib.get(b"Py_Initialize")?;
        initialize();

        debug!("Running Py_Main: {}", args.join(" "));
        // run python interpreter as from the cli
        // https://docs.python.org/3/c-api/veryhigh.html#c.Py_BytesMain
        let py_main: libloading::Symbol<unsafe extern "C" fn(c_int, *mut *const wchar_t) -> c_int> =
            lib.get(b"Py_Main")?;

        // env::args panics when there is a non utf-8 string, but converting OsString -> *c_char
        // is an even bigger mess
        let args_cstring: Vec<WideCString> = args
            .iter()
            .map(|arg| WideCString::from_str(arg).unwrap())
            .collect();
        let mut args_c_char: Vec<*const wchar_t> = args_cstring
            .iter()
            .map(|arg| arg.as_ptr() as *const wchar_t)
            .collect();
        let exit_code = py_main(args_cstring.len() as c_int, args_c_char.as_mut_ptr());
        // > The return value will be 0 if the interpreter exits normally (i.e., without an
        // > exception), 1 if the interpreter exits due to an exception, or 2 if the parameter list
        // > does not represent a valid Python command line.
        // >
        // > Note that if an otherwise unhandled SystemExit is raised, this function will not
        // > return 1, but exit the process, as long as Py_InspectFlag is not set.
        // Let the caller exit with that status if python didn't
        Ok(exit_code)
    }
}

fn run() -> Result<i32, PythonPlusPlusError> {
    // Skip the name of the rust binary
    let args: Vec<String> = env::args().collect();
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "System needs to have a cache dir"))?
        .join(env!("CARGO_PKG_NAME"));
    let default_python_version = (3, 10);
    let (args_after, python_version) =
        determine_python_version(&args[1..], None, default_python_version)
            .map_err(PythonPlusPlusError::DeterminePythonVersion)?;
    let (python_binary, python_home) = provision_python(python_version, &cache_dir)
        .map_err(PythonPlusPlusError::ProvisionPython)?;

    let Some(script) =
        naive_python_arg_parser(&args_after).map_err(PythonPlusPlusError::CpythonArgs)?
    else {
        return Err(PythonPlusPlusError::MissingScript);
    };

    let content = fs::read_to_string(&script)?;
    let formatted = format_module_source(&content, PyFormatOptions::default())?;
    let temp_file = NamedTempFile::new()?;
    fs::write(temp_file.path(), formatted.as_code())?;
    let temp_file_string_name = temp_file
        .path()
        .to_str()
        .ok_or_else(|| PythonPlusPlusError::InvalidPath(temp_file.path().to_path_buf()))?;

    // Don't look
    let args_after: Vec<String> = args_after
        .into_iter()
        .map(|arg| {
            if arg == script {
                temp_file_string_name.to_string()
            } else {
                arg
            }
        })
        .collect();

    let final_args: Vec<String> = [python_binary
        .to_str()
        .ok_or_else(|| PythonPlusPlusError::InvalidPath(python_binary.to_path_buf()))?
        .to_string()]
    .into_iter()
    .chain(args_after)
    .collect();

    debug!("Running cpython with {:?}", final_args);
    let exit_code =
        inject_and_run_python(&python_home, python_version, &python_binary, &final_args)?;

    Ok(exit_code)
}

fn main() {
    match run() {
        Err(err) => {
            eprintln!("💥 {} failed", env!("CARGO_PKG_NAME"));
            let mut last_error: Option<&(dyn Error + 'static)> = Some(&err);
            while let Some(err) = last_error {
                eprintln!("  Caused by: {err}");
                last_error = err.source();
            }
            std::process::exit(1);
        }
        Ok(exit_code) => {
            debug!("Exit code: {}", exit_code);
            std::process::exit(exit_code);
        }
    }
}
