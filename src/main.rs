use maturin::*;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use structopt::clap::AppSettings;
use structopt::StructOpt;
use walkdir::WalkDir;

/// Build python wheels
#[derive(Debug, StructOpt)]
struct Info {
    /// The name of the python module to create. This module name must match that of the library in
    /// the wheel or the wheel will fail when trying to import.
    ///
    /// For a "mixed" package (pure python + a compiled extension) this can be a dotted name, e.g.
    /// `my_pkg._native`, in which case the compiled extension is placed at `my_pkg/_native.*.so`
    /// inside the wheel, alongside the pure python sources. A value in pyproject.toml's
    /// `[tool.maturin] module-name` (see `--pyproject-path`) takes precedence over this flag.
    #[structopt(long = "module-name")]
    module_name: String,

    /// Assume the module is abi3 compatible. This tags the wheel with an abi3 tag, and also
    /// doesn't bother to tag the .so inside the wheel with any tag.
    #[structopt(long)]
    abi3: bool,

    /// Path to the Cargo.toml file. This file is used to provide the metadata for the python
    /// wheel. Be aware that if this points to readme file, that readme file should also be in the
    /// same folder.
    #[structopt(long = "manifest-path")]
    manifest_path: PathBuf,

    /// Path to a pyproject.toml file. When given, its `[tool.maturin]` table is read for
    /// `module-name` and `python-source`, mirroring upstream maturin's "mixed" project layout so
    /// the same config can drive both `maturin develop` (in development) and this tool (for nix
    /// packaging). `python-source` is resolved relative to the pyproject.toml's directory and its
    /// entire tree is copied into the wheel.
    #[structopt(long = "pyproject-path")]
    pyproject_path: Option<PathBuf>,
}

/// The subset of `[tool.maturin]` we understand.
#[derive(Debug, Default, Deserialize)]
struct MaturinConfig {
    #[serde(rename = "module-name")]
    module_name: Option<String>,
    #[serde(rename = "python-source")]
    python_source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Tool {
    maturin: Option<MaturinConfig>,
}

#[derive(Debug, Deserialize)]
struct PyProject {
    tool: Option<Tool>,
}

impl Info {
    fn meta21(&self) -> Metadata21 {
        let cargo_toml = CargoToml::from_path(&self.manifest_path).expect("manifest_file");

        // The manifest directory is only used when the target toml file points to a readme.
        let manifest_dir = self.manifest_path.parent().unwrap();

        Metadata21::from_cargo_toml(&cargo_toml, &manifest_dir).expect("metadata21")
    }

    /// Resolve the final module name and (optional) pure-python source directory, taking
    /// `[tool.maturin]` from pyproject.toml into account where present.
    fn resolve_layout(&self) -> (String, Option<PathBuf>) {
        match &self.pyproject_path {
            None => (self.module_name.clone(), None),
            Some(pyproject_path) => {
                let contents = std::fs::read_to_string(pyproject_path).unwrap_or_else(|e| {
                    panic!("failed to read {}: {}", pyproject_path.display(), e)
                });
                let pyproject: PyProject = toml::from_str(&contents).unwrap_or_else(|e| {
                    panic!("failed to parse {}: {}", pyproject_path.display(), e)
                });
                let config = pyproject.tool.and_then(|t| t.maturin).unwrap_or_default();

                let module_name = config
                    .module_name
                    .unwrap_or_else(|| self.module_name.clone());

                let pyproject_dir = pyproject_path.parent().unwrap_or_else(|| Path::new("."));
                let python_source = config.python_source.map(|src| pyproject_dir.join(src));

                (module_name, python_source)
            }
        }
    }
}

/// Build python wheels
#[derive(Debug, StructOpt)]
#[structopt(
    name = "maturin-nix",
    about = "Tool for building pyo3 wheels inside nix",
    global_settings(&[AppSettings::ColoredHelp, AppSettings::VersionlessSubcommands])
)]

enum Opt {
    #[structopt(name = "build")]
    /// Build the crate into wheels
    Build {
        #[structopt(flatten)]
        info: Info,

        /// The path to the rustc artifact for a library. This library must have a crate-type of
        /// "cdylib". On macOS the library should also be compiled with
        ///  "-C link-arg=-undefined -C link-arg=dynamic_lookup";
        #[structopt(long)]
        artifact_path: PathBuf,

        /// The directory to store the output wheel.
        #[structopt(long)]
        output_dir: PathBuf,
    },
}

/// Compute where the compiled extension's `.so` should live inside the wheel, given a (possibly
/// dotted) module name. For `my_pkg._native` this is `my_pkg/_native.<tag>.so`; for a plain
/// `my_pkg` it is the top-level `my_pkg.<tag>.so`
fn so_target(module_name: &str, py: &Option<PythonInterpreter>) -> PathBuf {
    let mut parts: Vec<&str> = module_name.split('.').collect();
    // `split` always yields at least one element, so this never panics.
    let extension_name = parts.pop().unwrap();

    let filename = match py {
        Some(py) => py.get_library_name(extension_name),
        // Assumes Unix.
        None => format!("{}.so", extension_name),
    };

    let mut path = PathBuf::new();
    for part in parts {
        path.push(part);
    }
    path.push(filename);
    path
}

fn main() {
    let opt = Opt::from_args();

    match opt {
        Opt::Build {
            info,
            artifact_path,
            output_dir,
        } => {
            let (module_name, python_source) = info.resolve_layout();

            let build_wheel = |py: &Option<PythonInterpreter>| {
                let tag = if let Some(py) = py {
                    // manylinux basically says that there should be a bunch of standard libraries in standard
                    // places. This doesn't play nicely with nix so we don't use it.
                    py.get_tag(&Manylinux::Off)
                } else {
                    // Oldest supported (listed in docs) by pyo3
                    let python_tag = "cp37";
                    let abi_tag = "abi3";
                    let platform_tag = "linux_x86_64";
                    format!("{}-{}-{}", python_tag, abi_tag, platform_tag)
                };

                let mut writer = WheelWriter::new(
                    &tag,
                    &output_dir,
                    &info.meta21(),
                    &std::collections::HashMap::default(),
                    &[tag.clone()],
                )
                .expect("writer");

                // Add the pure python part of a mixed package, preserving paths relative to the
                // python source directory
                if let Some(python_source) = &python_source {
                    for entry in WalkDir::new(python_source) {
                        let entry = entry.expect("walk python source");
                        if !entry.file_type().is_file() {
                            continue;
                        }
                        let absolute = entry.path();
                        // Don't include python bytecode caches
                        if absolute
                            .components()
                            .any(|c| c.as_os_str() == "__pycache__")
                            || absolute.extension().is_some_and(|e| e == "pyc")
                        {
                            continue;
                        }
                        let relative = absolute
                            .strip_prefix(python_source)
                            .expect("python source prefix");
                        writer
                            .add_file(relative, absolute)
                            .expect("add python file");
                    }
                }

                let so_filename = so_target(&module_name, py);

                writer
                    .add_file(&so_filename, &artifact_path)
                    .expect("add files");

                let wheel_path = writer.finish().expect("writer finish");

                eprintln!("📦 successfully created wheel {}", wheel_path.display());
            };

            if info.abi3 {
                build_wheel(&None);
            } else {
                let target = Target::current();
                let bridge = BridgeModel::Cffi;
                println!("Looking for interpreters...");
                let python_interpreters =
                    PythonInterpreter::find_all(&target, &bridge).expect("python_interpreter");

                if python_interpreters.is_empty() {
                    panic!("Couldn't find any recognised Python interpreters")
                }

                for py in python_interpreters {
                    println!("Found {:?}", py);
                    build_wheel(&Some(py));
                }
            }
        }
    }
}
