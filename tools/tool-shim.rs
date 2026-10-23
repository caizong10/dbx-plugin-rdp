// Generic MinGW tool shim: the DBX dev host spawns cargo with verbatim
// (`\\?\D:\...`) working paths, and cc-rs forwards those verbatim paths to the
// C toolchain (gcc/as/ar/ld/dlltool), which GNU tools read as invalid UNC.
//
// This binary is copied into tools/shim-bin/{gcc,as,ar,ld,dlltool}.exe. Each
// copy looks at its own file name, strips `\\?\` from every argument (and from
// @response files), prepends the real MinGW bin directory to PATH, and execs
// the same-named real tool. rustc's configured linker uses tools/linker-shim
// for the rustc-side calls; this shim covers build-script cc invocations.
//
// The MinGW bin directory is resolved at run time: DBX_MINGW_BIN if set, else
// the PATH entry that contains gcc.exe. No machine path is baked in.
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{exit, Command};

fn real_bin_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("DBX_MINGW_BIN") {
        return PathBuf::from(explicit);
    }
    for entry in std::env::split_paths(&std::env::var("PATH").unwrap_or_default()) {
        if entry.join("gcc.exe").is_file() {
            return entry;
        }
    }
    eprintln!("tool-shim: gcc.exe not on PATH; set DBX_MINGW_BIN");
    exit(1);
}

fn strip_verbatim(text: &str) -> String {
    text.replace(r"\\\\?\\", "").replace(r"\\?\", "")
}

fn deverbatim(argument: OsString) -> OsString {
    let text = argument.to_string_lossy();
    if text.contains(r"\\?\") {
        OsString::from(strip_verbatim(&text))
    } else {
        argument
    }
}

fn rewrite_response_file(original: &str) -> Result<PathBuf, String> {
    let content = fs::read_to_string(original).map_err(|e| format!("read {original}: {e}"))?;
    let fixed = strip_verbatim(&content);
    let path = PathBuf::from(format!("{original}.deverbatim.txt"));
    let mut file = fs::File::create(&path).map_err(|e| format!("create temp: {e}"))?;
    file.write_all(fixed.as_bytes()).map_err(|e| format!("write temp: {e}"))?;
    Ok(path)
}

fn real_target(bin_dir: &Path, ) -> PathBuf {
    let self_name = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "gcc".to_string());
    // Copy names are plain tool names (gcc, as, ar, ld, dlltool).
    bin_dir.join(format!("{self_name}.exe"))
}

fn main() {
    let bin_dir = real_bin_dir();
    let mut args: Vec<OsString> = Vec::new();
    for argument in std::env::args_os().skip(1) {
        let stripped = deverbatim(argument);
        let text = stripped.to_string_lossy().to_string();
        if let Some(response) = text.strip_prefix('@') {
            match rewrite_response_file(&response) {
                Ok(temp) => args.push(OsString::from(format!("@{}", temp.display()))),
                Err(error) => {
                    eprintln!("tool-shim: {error}");
                    args.push(OsString::from(text));
                }
            }
        } else {
            args.push(stripped);
        }
    }

    let target = real_target(&bin_dir);
    let mut command = Command::new(&target);
    command.args(&args);
    if let Ok(path) = std::env::var("PATH") {
        command.env("PATH", format!("{};{path}", bin_dir.display()));
    } else {
        command.env("PATH", &bin_dir);
    }
    match command.status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("tool-shim: cannot launch {}: {error}", target.display());
            exit(1);
        }
    }
}
