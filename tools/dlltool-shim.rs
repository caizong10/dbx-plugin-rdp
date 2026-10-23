// Shim: rustc invokes dlltool.exe with verbatim (`\\?\D:\...`) paths that the
// GNU dlltool treats as UNC and cannot open. This wrapper strips the prefix
// from every argument (and @response files), injects the real toolchain bin
// directory into PATH, and forwards to the genuine dlltool.
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{exit, Command};

// Real MinGW bin directory: DBX_MINGW_BIN if set, else the PATH entry that
// actually contains dlltool.exe. No machine path is baked into the source.
fn real_bin_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("DBX_MINGW_BIN") {
        return PathBuf::from(explicit);
    }
    for entry in std::env::split_paths(&std::env::var("PATH").unwrap_or_default()) {
        if entry.join("dlltool.exe").is_file() {
            return entry;
        }
    }
    eprintln!("dlltool-shim: dlltool.exe not on PATH; set DBX_MINGW_BIN");
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

fn main() {
    let mut args: Vec<OsString> = Vec::new();
    for argument in std::env::args_os().skip(1) {
        let stripped = deverbatim(argument);
        let text = stripped.to_string_lossy().to_string();
        if let Some(response) = text.strip_prefix('@') {
            match rewrite_response_file(&response) {
                Ok(temp) => args.push(OsString::from(format!("@{}", temp.display()))),
                Err(error) => {
                    eprintln!("dlltool-shim: {error}");
                    args.push(OsString::from(text));
                }
            }
        } else {
            args.push(stripped);
        }
    }

    let bin_dir = real_bin_dir();
    let mut command = Command::new(bin_dir.join("dlltool.exe"));
    command.args(&args);
    if let Ok(path) = std::env::var("PATH") {
        command.env("PATH", format!("{};{path}", bin_dir.display()));
    } else {
        command.env("PATH", &bin_dir);
    }
    match command.status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("dlltool-shim: cannot launch real dlltool: {error}");
            exit(1);
        }
    }
}
