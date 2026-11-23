//! Optional trusted Python actions, hosted in a separate process.
use super::{Budget, error};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(super) fn execute(
    source: &str,
    project: &str,
    context: &str,
    budget: &Budget,
    executable: Option<&std::path::Path>,
) -> Result<String, String> {
    let custom = executable
        .map(|p| p.as_os_str().to_owned())
        .or_else(|| std::env::var_os("PEEL_PYTHON"));
    let bundled = if custom.is_none() {
        bundled_python()?
    } else {
        None
    };
    let is_bundled = bundled.is_some();
    let executable = custom
        .or_else(|| bundled.map(PathBuf::into_os_string))
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "python".into()
            } else {
                "python3".into()
            }
        });
    // Build input before starting a child so serialization failures cannot leak it.
    let payload = json!({"source":source,"project":serde_json::from_str::<serde_json::Value>(project).map_err(error)?,"context":serde_json::from_str::<serde_json::Value>(context).map_err(error)?}).to_string();
    let mut command = Command::new(executable);
    command.args(["-I", "-B", "-u", "-c", include_str!("python_host.py")]);
    let output = super::process::execute(command, payload, budget).map_err(|e| {
        if is_bundled {
            format!("Bundled Python: {e}. Reinstall Peachey Studio if its runtime is damaged.")
        } else {
            format!("Python: {e}. Choose a Python interpreter, or use a packaged Peachey Studio release with bundled PyTheory.")
        }
    })?;
    let mut result: serde_json::Value =
        serde_json::from_slice(&output).map_err(|e| format!("Invalid Python result: {e}"))?;
    if let Some(logs) = result.get("logs").and_then(|l| l.as_array()) {
        for line in logs {
            if let Some(text) = line.as_str() {
                budget.log(text.into());
            }
        }
    }
    serde_json::to_string(&result["project"].take()).map_err(error)
}

/// Runtime locations follow the package, so moving the application keeps Python working.
fn bundled_at(executable: &Path) -> Result<Option<PathBuf>, String> {
    let Some(directory) = executable.parent() else {
        return Ok(None);
    };
    let roots = [
        directory.join("../Resources/python"),
        directory.join("python"),
        directory.join("../lib/peel/python"),
    ];
    for root in roots {
        if !root.join("peel-python.json").is_file() {
            continue;
        }
        let python = root.join(if cfg!(windows) {
            "python.exe"
        } else {
            "bin/python3"
        });
        if !python.is_file() {
            return Err(
                "The bundled Python runtime is incomplete. Reinstall Peachey Studio.".into(),
            );
        }
        return Ok(Some(python));
    }
    Ok(None)
}

pub fn bundled_python() -> Result<Option<PathBuf>, String> {
    bundled_at(&std::env::current_exe().map_err(error)?)
}

/// A short UI label for the automatic interpreter choice.
pub fn default_python_label() -> &'static str {
    if std::env::var_os("PEEL_PYTHON").is_some() {
        return "Custom Python";
    }
    match bundled_python() {
        Ok(Some(_)) => "Bundled Python",
        Ok(None) => "System Python",
        Err(_) => "Python unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_relocated_packages_and_refuses_incomplete_runtimes() {
        let root = std::env::temp_dir().join(format!("peel-runtime-{}", peel_document::Id::new()));
        for (binary, runtime) in [
            (
                "Peachey Studio.app/Contents/MacOS/peachey-studio",
                "Peachey Studio.app/Contents/Resources/python",
            ),
            ("portable/peachey-studio", "portable/python"),
            ("linux/bin/peachey-studio", "linux/lib/peel/python"),
        ] {
            let binary = root.join(binary);
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            assert!(bundled_at(&binary).unwrap().is_none());
            let runtime = root.join(runtime);
            std::fs::create_dir_all(&runtime).unwrap();
            std::fs::write(runtime.join("peel-python.json"), "{}").unwrap();
            assert!(bundled_at(&binary).is_err());
            let python = runtime.join(if cfg!(windows) {
                "python.exe"
            } else {
                "bin/python3"
            });
            std::fs::create_dir_all(python.parent().unwrap()).unwrap();
            std::fs::write(&python, "placeholder").unwrap();
            assert_eq!(
                std::fs::canonicalize(bundled_at(&binary).unwrap().unwrap()).unwrap(),
                std::fs::canonicalize(python).unwrap()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
