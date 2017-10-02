//! A build dependency for Cargo libraries to find system artifacts through the
//! `pkg-config` utility.
//!
//! This library will shell out to `pkg-config` as part of build scripts and
//! probe the system to determine how to link to a specified library. The
//! `Config` structure serves as a method of configuring how `pkg-config` is
//! invoked in a builder style.
//!
//! A number of environment variables are available to globally configure how
//! this crate will invoke `pkg-config`:
//!
//! * `PKG_CONFIG_ALLOW_CROSS` - if this variable is not set, then `pkg-config`
//!   will automatically be disabled for all cross compiles.
//! * `FOO_NO_PKG_CONFIG` - if set, this will disable running `pkg-config` when
//!   probing for the library named `foo`.
//!
//! There are also a number of environment variables which can configure how a
//! library is linked to (dynamically vs statically). These variables control
//! whether the `--static` flag is passed. Note that this behavior can be
//! overridden by configuring explicitly on `Config`. The variables are checked
//! in the following order:
//!
//! * `FOO_STATIC` - pass `--static` for the library `foo`
//! * `FOO_DYNAMIC` - do not pass `--static` for the library `foo`
//! * `PKG_CONFIG_ALL_STATIC` - pass `--static` for all libraries
//! * `PKG_CONFIG_ALL_DYNAMIC` - do not pass `--static` for all libraries
//!
//! After running `pkg-config` all appropriate Cargo metadata will be printed on
//! stdout if the search was successful.
//!
//! # Example
//!
//! Find the system library named `foo`, with minimum version 1.2.3:
//!
//! ```no_run
//! extern crate pkg_config;
//!
//! fn main() {
//!     pkg_config::Config::new().atleast_version("1.2.3").probe("foo").unwrap();
//! }
//! ```
//!
//! Find the system library named `foo`, with no version requirement (not
//! recommended):
//!
//! ```no_run
//! extern crate pkg_config;
//!
//! fn main() {
//!     pkg_config::probe_library("foo").unwrap();
//! }
//! ```
//!
//! Configure how library `foo` is linked to.
//!
//! ```no_run
//! extern crate pkg_config;
//!
//! fn main() {
//!     pkg_config::Config::new().atleast_version("1.2.3").statik(true).probe("foo").unwrap();
//! }
//! ```

#![doc(html_root_url = "https://docs.rs/pkg-config/0.3")]
#![cfg_attr(test, deny(warnings))]

use std::ascii::AsciiExt;
use std::env;
use std::error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{PathBuf, Path};
use std::process::{Command, Output};
use std::str;

pub fn target_supported() -> bool {
    let target = env::var("TARGET").unwrap_or(String::new());
    let host = env::var("HOST").unwrap_or(String::new());

    // Only use pkg-config in host == target situations by default (allowing an
    // override) and then also don't use pkg-config on MSVC as it's really not
    // meant to work there but when building MSVC code in a MSYS shell we may be
    // able to run pkg-config anyway.
    (host == target || get_var_os("PKG_CONFIG_ALLOW_CROSS").is_some()) &&
    !target.contains("msvc")
}

#[derive(Clone)]
pub struct Config {
    statik: Option<bool>,
    atleast_version: Option<String>,
    extra_args: Vec<OsString>,
    cargo_metadata: bool,
    print_system_libs: bool,
}

#[derive(Debug)]
pub struct Library {
    pub libs: Vec<String>,
    pub link_paths: Vec<PathBuf>,
    pub frameworks: Vec<String>,
    pub framework_paths: Vec<PathBuf>,
    pub include_paths: Vec<PathBuf>,
    pub version: String,
    _priv: (),
}

/// Represents all reasons `pkg-config` might not succeed or be run at all.
pub enum Error {
    /// Aborted because of `*_NO_PKG_CONFIG` environment variable.
    ///
    /// Contains the name of the responsible environment variable.
    EnvNoPkgConfig(String),

    /// Cross compilation detected.
    ///
    /// Override with `PKG_CONFIG_ALLOW_CROSS=1`.
    CrossCompilation,

    /// Attempted to compile using the MSVC ABI build
    MSVC,

    /// Failed to run `pkg-config`.
    ///
    /// Contains the command and the cause.
    Command { command: String, cause: io::Error },

    /// `pkg-config` did not exit sucessfully.
    ///
    /// Contains the command and output.
    Failure { command: String, output: Output },

    #[doc(hidden)]
    // please don't match on this, we're likely to add more variants over time
    __Nonexhaustive,
}

impl error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::EnvNoPkgConfig(_) => "pkg-config requested to be aborted",
            Error::CrossCompilation => {
                "pkg-config doesn't handle cross compilation. \
                 Use PKG_CONFIG_ALLOW_CROSS=1 to override"
            }
            Error::MSVC => "pkg-config is incompatible with the MSVC ABI build.",
            Error::Command { .. } => "failed to run pkg-config",
            Error::Failure { .. } => "pkg-config did not exit sucessfully",
            Error::__Nonexhaustive => panic!(),
        }
    }

    fn cause(&self) -> Option<&error::Error> {
        match *self {
            Error::Command { ref cause, .. } => Some(cause),
            _ => None,
        }
    }
}

// Workaround for temporary lack of impl Debug for Output in stable std
struct OutputDebugger<'a>(&'a Output);

// Lifted from 1.7 std
impl<'a> fmt::Debug for OutputDebugger<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let stdout_utf8 = str::from_utf8(&self.0.stdout);
        let stdout_debug: &fmt::Debug = match stdout_utf8 {
            Ok(ref str) => str,
            Err(_) => &self.0.stdout
        };

        let stderr_utf8 = str::from_utf8(&self.0.stderr);
        let stderr_debug: &fmt::Debug = match stderr_utf8 {
            Ok(ref str) => str,
            Err(_) => &self.0.stderr
        };

        fmt.debug_struct("Output")
           .field("status", &self.0.status)
           .field("stdout", stdout_debug)
           .field("stderr", stderr_debug)
           .finish()
    }
}

// Workaround for temporary lack of impl Debug for Output in stable std, continued
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            Error::EnvNoPkgConfig(ref name) => {
                f.debug_tuple("EnvNoPkgConfig")
                 .field(name)
                 .finish()
            }
            Error::CrossCompilation => write!(f, "CrossCompilation"),
            Error::MSVC => write!(f, "MSVC"),
            Error::Command { ref command, ref cause } => {
                f.debug_struct("Command")
                 .field("command", command)
                 .field("cause", cause)
                 .finish()
            }
            Error::Failure { ref command, ref output } => {
                f.debug_struct("Failure")
                 .field("command", command)
                 .field("output", &OutputDebugger(output))
                 .finish()
            }
            Error::__Nonexhaustive => panic!(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            Error::EnvNoPkgConfig(ref name) => {
                write!(f, "Aborted because {} is set", name)
            }
            Error::CrossCompilation => {
                write!(f, "Cross compilation detected. \
                       Use PKG_CONFIG_ALLOW_CROSS=1 to override")
            }
            Error::MSVC => {
                write!(f, "MSVC target detected. If you are using the MSVC ABI \
                       rust build, please use the GNU ABI build instead.")
            }
            Error::Command { ref command, ref cause } => {
                write!(f, "Failed to run `{}`: {}", command, cause)
            }
            Error::Failure { ref command, ref output } => {
                let stdout = str::from_utf8(&output.stdout).unwrap();
                let stderr = str::from_utf8(&output.stderr).unwrap();
                try!(write!(f, "`{}` did not exit successfully: {}", command, output.status));
                if !stdout.is_empty() {
                    try!(write!(f, "\n--- stdout\n{}", stdout));
                }
                if !stderr.is_empty() {
                    try!(write!(f, "\n--- stderr\n{}", stderr));
                }
                Ok(())
            }
            Error::__Nonexhaustive => panic!(),
        }
    }
}

/// Deprecated in favor of the probe_library function
#[doc(hidden)]
pub fn find_library(name: &str) -> Result<Library, String> {
    probe_library(name).map_err(|e| e.to_string())
}

/// Simple shortcut for using all default options for finding a library.
pub fn probe_library(name: &str) -> Result<Library, Error> {
    Config::new().probe(name)
}

/// Run `pkg-config` to get the value of a variable from a package using
/// --variable.
pub fn get_variable(package: &str, variable: &str) -> Result<String, Error> {
    let arg = format!("--variable={}", variable);
    let cfg = Config::new();
    Ok(try!(run(cfg.command(package, &[&arg]))).trim_right().to_owned())
}

impl Config {
    /// Creates a new set of configuration options which are all initially set
    /// to "blank".
    pub fn new() -> Config {
        Config {
            statik: None,
            atleast_version: None,
            extra_args: vec![],
            print_system_libs: true,
            cargo_metadata: true,
        }
    }

    /// Indicate whether the `--static` flag should be passed.
    ///
    /// This will override the inference from environment variables described in
    /// the crate documentation.
    pub fn statik(&mut self, statik: bool) -> &mut Config {
        self.statik = Some(statik);
        self
    }

    /// Indicate that the library must be at least version `vers`.
    pub fn atleast_version(&mut self, vers: &str) -> &mut Config {
        self.atleast_version = Some(vers.to_string());
        self
    }

    /// Add an argument to pass to pkg-config.
    ///
    /// It's placed after all of the arguments generated by this library.
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Config {
        self.extra_args.push(arg.as_ref().to_os_string());
        self
    }

    /// Define whether metadata should be emitted for cargo allowing it to
    /// automatically link the binary. Defaults to `true`.
    pub fn cargo_metadata(&mut self, cargo_metadata: bool) -> &mut Config {
        self.cargo_metadata = cargo_metadata;
        self
    }

    /// Enable or disable the `PKG_CONFIG_ALLOW_SYSTEM_LIBS` environment
    /// variable.
    ///
    /// This env var is enabled by default.
    pub fn print_system_libs(&mut self, print: bool) -> &mut Config {
        self.print_system_libs = print;
        self
    }

    /// Deprecated in favor fo the `probe` function
    #[doc(hidden)]
    pub fn find(&self, name: &str) -> Result<Library, String> {
        self.probe(name).map_err(|e| e.to_string())
    }

    /// Run `pkg-config` to find the library `name`.
    ///
    /// This will use all configuration previously set to specify how
    /// `pkg-config` is run.
    pub fn probe(&self, name: &str) -> Result<Library, Error> {
        let abort_var_name = format!("{}_NO_PKG_CONFIG", envify(name));
        if get_var_os(&abort_var_name).is_some() {
            return Err(Error::EnvNoPkgConfig(abort_var_name))
        } else if !target_supported() {
            if env::var("TARGET").unwrap_or(String::new()).contains("msvc") {
                return Err(Error::MSVC);
            }
            else {
                return Err(Error::CrossCompilation);
            }
        }

        // pkg-config can be configured via env vars,
        // and should be probed again if the configuration changes.
        // Unfortunately, this list is not exhaustive,
        // because arbitrary `PKG_CONFIG_$PKG_$VAR` can exist.
        // NB: `get_var_os` automatically marks vars as used.
        rerun_if_changed("PKG_CONFIG_PATH");
        rerun_if_changed("PKG_CONFIG_SYSTEM_INCLUDE_PATH");
        rerun_if_changed("CPATH");
        rerun_if_changed("INCLUDE");
        rerun_if_changed("PKG_CONFIG_ALLOW_SYSTEM_CFLAGS");
        rerun_if_changed("PKG_CONFIG_SYSTEM_LIBRARY_PATH");
        rerun_if_changed("PKG_CONFIG_ALLOW_SYSTEM_LIBS");
        rerun_if_changed("PKG_CONFIG_SYSROOT_DIR");
        rerun_if_changed("PKG_CONFIG_LIBDIR");
        rerun_if_changed("PKG_CONFIG_DISABLE_UNINSTALLED");

        let mut library = Library::new();

        let output = try!(run(self.command(name, &["--libs", "--cflags"])));
        library.parse_libs_cflags(name, &output, self);

        let output = try!(run(self.command(name, &["--modversion"])));
        library.parse_modversion(&output);

        Ok(library)
    }

    /// Deprecated in favor of the top level `get_variable` function
    #[doc(hidden)]
    pub fn get_variable(package: &str, variable: &str) -> Result<String, String> {
        get_variable(package, variable).map_err(|e| e.to_string())
    }

    fn is_static(&self, name: &str) -> bool {
        self.statik.unwrap_or_else(|| infer_static(name))
    }

    fn command(&self, name: &str, args: &[&str]) -> Command {
        let exe = get_var_os("PKG_CONFIG").unwrap_or(OsString::from("pkg-config"));
        let mut cmd = Command::new(exe);
        if self.is_static(name) {
            cmd.arg("--static");
        }
        cmd.args(args)
           .args(&self.extra_args);

        if self.print_system_libs {
            cmd.env("PKG_CONFIG_ALLOW_SYSTEM_LIBS", "1");
        }
        if let Some(ref version) = self.atleast_version {
            cmd.arg(&format!("{} >= {}", name, version));
        } else {
            cmd.arg(name);
        }
        cmd
    }

    fn print_metadata(&self, s: &str) {
        if self.cargo_metadata {
            println!("cargo:{}", s);
        }
    }
}

impl Library {
    fn new() -> Library {
        Library {
            libs: Vec::new(),
            link_paths: Vec::new(),
            include_paths: Vec::new(),
            frameworks: Vec::new(),
            framework_paths: Vec::new(),
            version: String::new(),
            _priv: (),
        }
    }

    fn parse_libs_cflags(&mut self, name: &str, output: &str, config: &Config) {
        let parts = output.trim_right()
                          .split(' ')
                          .filter(|l| l.len() > 2)
                          .map(|arg| (&arg[0..2], &arg[2..]))
                          .collect::<Vec<_>>();

        let mut dirs = Vec::new();
        let statik = config.is_static(name);
        for &(flag, val) in parts.iter() {
            match flag {
                "-L" => {
                    let meta = format!("rustc-link-search=native={}", val);
                    config.print_metadata(&meta);
                    dirs.push(PathBuf::from(val));
                    self.link_paths.push(PathBuf::from(val));
                }
                "-F" => {
                    let meta = format!("rustc-link-search=framework={}", val);
                    config.print_metadata(&meta);
                    self.framework_paths.push(PathBuf::from(val));
                }
                "-I" => {
                    self.include_paths.push(PathBuf::from(val));
                }
                "-l" => {
                    self.libs.push(val.to_string());
                    if statik && is_static_available(val, &dirs) {
                        let meta = format!("rustc-link-lib=static={}", val);
                        config.print_metadata(&meta);
                    } else {
                        let meta = format!("rustc-link-lib={}", val);
                        config.print_metadata(&meta);
                    }
                }
                _ => {}
            }
        }

        let mut iter = output.trim_right().split(' ');
        while let Some(part) = iter.next() {
            if part != "-framework" {
                continue
            }
            if let Some(lib) = iter.next() {
                let meta = format!("rustc-link-lib=framework={}", lib);
                config.print_metadata(&meta);
                self.frameworks.push(lib.to_string());
            }
        }
    }

    fn parse_modversion(&mut self, output: &str) {
        self.version.push_str(output.trim());
    }
}

fn infer_static(name: &str) -> bool {
    let name = envify(name);
    if get_var_os(&format!("{}_STATIC", name)).is_some() {
        true
    } else if get_var_os(&format!("{}_DYNAMIC", name)).is_some() {
        false
    } else if get_var_os("PKG_CONFIG_ALL_STATIC").is_some() {
        true
    } else if get_var_os("PKG_CONFIG_ALL_DYNAMIC").is_some() {
        false
    } else {
        false
    }
}

fn envify(name: &str) -> String {
    name.chars().map(|c| c.to_ascii_uppercase()).map(|c| {
        if c == '-' {'_'} else {c}
    }).collect()
}

/// Mark build as dependent on the variable
fn rerun_if_changed(var_name: &str) {
    println!("cargo:rerun-if-env-changed={}", var_name);
}

/// Get variable and mark the build as dependent on it
fn get_var_os(var_name: &str) -> Option<OsString> {
    rerun_if_changed(var_name);
    env::var_os(var_name)
}

/// System libraries should only be linked dynamically
fn is_static_available(name: &str, dirs: &[PathBuf]) -> bool {
    let libname = format!("lib{}.a", name);
    let system_roots = if cfg!(target_os = "macos") {
        vec![Path::new("/Library"), Path::new("/System")]
    } else {
        vec![Path::new("/usr")]
    };

    dirs.iter().any(|dir| {
        !system_roots.iter().any(|sys| dir.starts_with(sys)) &&
        dir.join(&libname).exists()
    })
}

fn run(mut cmd: Command) -> Result<String, Error> {
    match cmd.output() {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8(output.stdout).unwrap();
                Ok(stdout)
            } else {
                Err(Error::Failure {
                    command: format!("{:?}", cmd),
                    output: output,
                })
            }
        }
        Err(cause) => Err(Error::Command {
            command: format!("{:?}", cmd),
            cause: cause,
        }),
    }
}

#[test]
#[cfg(target_os = "macos")]
fn system_library_mac_test() {
    assert!(!is_static_available("PluginManager", &[PathBuf::from("/Library/Frameworks")]));
    assert!(!is_static_available("python2.7", &[PathBuf::from("/System/Library/Frameworks/Python.framework/Versions/2.7/lib/python2.7/config")]));
    assert!(!is_static_available("ffi_convenience", &[PathBuf::from("/Library/Ruby/Gems/2.0.0/gems/ffi-1.9.10/ext/ffi_c/libffi-x86_64/.libs")]));

    // Homebrew is in /usr/local, and it's not a part of the OS
    if Path::new("/usr/local/lib/libpng16.a").exists() {
        assert!(is_static_available("png16", &[PathBuf::from("/usr/local/lib")]));
    }
}

#[test]
#[cfg(target_os = "linux")]
fn system_library_linux_test() {
    assert!(!is_static_available("util", &[PathBuf::from("/usr/lib/x86_64-linux-gnu")]));
    assert!(!is_static_available("dialog", &[PathBuf::from("/usr/lib")]));
}
