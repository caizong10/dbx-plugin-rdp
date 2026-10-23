// Workaround for dbx-plugin dev/package on x86_64-pc-windows-gnu.
//
// Three separate problems, one shim:
//  A. The DBX CLI invokes cargo with verbatim paths (`\\?\D:\...`). The bundled
//     GNU ld treats them as UNC and cannot find object files or search roots.
//     The shim strips the prefix from direct args and rewrites gcc
//     @response-files through temp copies (both raw and doubled-backslash
//     escaped forms).
//  B. When rustc is pointed at a custom linker it assumes a full mingw-w64
//     install and drops the self-contained CRT args (explicit crt2.o /
//     dllcrt2.o, -nostartfiles, -L <self-contained lib root>). The shim
//     re-adds exactly what the default self-contained invocation passed,
//     choosing the object based on whether the link is -shared — a flag that
//     may live inside the @response file, not in argv.
//  C. The real gcc driver needs its sibling bin directory on PATH to spawn ld.
//
// Nothing about this machine is baked in: the toolchain layout is discovered
// at run time from the environment (override with DBX_GNU_TOOLCHAIN_ROOT).
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{exit, Command};

const TRIPLE: &str = "x86_64-pc-windows-gnu";

/// Root of the installed GNU rustup toolchain, e.g.
/// `<home>/.rustup/toolchains/stable-<triple>/lib/rustlib/<triple>`.
fn toolchain_root() -> PathBuf {
    if let Ok(explicit) = std::env::var("DBX_GNU_TOOLCHAIN_ROOT") {
        return PathBuf::from(explicit);
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    if home.is_empty() {
        eprintln!("linker-shim: no home directory; set DBX_GNU_TOOLCHAIN_ROOT");
        exit(1);
    }
    PathBuf::from(home)
        .join(".rustup")
        .join("toolchains")
        .join(format!("stable-{TRIPLE}"))
        .join("lib")
        .join("rustlib")
        .join(TRIPLE)
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

/// Rewrites a gcc @response file so no path keeps the verbatim prefix.
/// Returns the replacement path together with the rewritten content.
fn rewrite_response_file(original: &str) -> Result<(PathBuf, String), String> {
    let content = fs::read_to_string(original).map_err(|e| format!("read {original}: {e}"))?;
    let fixed = strip_verbatim(&content);
    let path = PathBuf::from(format!("{original}.deverbatim.txt"));
    let mut file = fs::File::create(&path).map_err(|e| format!("create temp: {e}"))?;
    file.write_all(fixed.as_bytes()).map_err(|e| format!("write temp: {e}"))?;
    Ok((path, fixed))
}

fn response_tokens(args: &[OsString]) -> Vec<String> {
    let mut tokens = Vec::new();
    for argument in args {
        let text = argument.to_string_lossy().to_string();
        if let Some(response) = text.strip_prefix('@') {
            if let Ok(content) = fs::read_to_string(response) {
                tokens.push(content);
            }
        } else {
            tokens.push(text);
        }
    }
    tokens
}

fn pick_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|path| path.is_file()).cloned()
}

fn main() {
    if let Ok(log_path) = std::env::var("DBX_LINKER_SHIM_LOG") {
        let raw: Vec<String> = std::env::args_os()
            .skip(1)
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let line = format!("{}\n", raw.join(" | "));
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = file.write_all(line.as_bytes());
        }
    }

    let root = toolchain_root();
    let self_contained = root.join("bin").join("self-contained");
    let crt_dir = root.join("lib").join("self-contained");
    let real_linker = pick_existing(&[
        self_contained.join(format!("x86_64-w64-mingw32-gcc.exe")),
        self_contained.join("gcc.exe"),
    ]);
    let Some(real_linker) = real_linker else {
        eprintln!("linker-shim: no GNU gcc under {}", self_contained.display());
        exit(1);
    };

    let mut args: Vec<OsString> = Vec::new();
    for argument in std::env::args_os().skip(1) {
        let stripped = deverbatim(argument);
        let text = stripped.to_string_lossy().to_string();
        if let Some(response) = text.strip_prefix('@') {
            match rewrite_response_file(&response) {
                Ok((temp, _)) => args.push(OsString::from(format!("@{}", temp.display()))),
                Err(error) => {
                    eprintln!("linker-shim: {error}");
                    args.push(OsString::from(text));
                }
            }
        } else {
            args.push(stripped);
        }
    }

    // (B) restore self-contained CRT args when rustc omitted them. Flags may
    // hide inside @response files, so expand every argument source first.
    let expanded = response_tokens(&args);
    let has_nostartfiles = expanded.iter().any(|token| token.contains("-nostartfiles"));
    let is_shared = expanded.iter().any(|token| {
        token == "-shared"
            || token.split_whitespace().any(|t| t == "-shared")
            || token.lines().any(|line| line.trim() == "\"-shared\"")
    });
    if !has_nostartfiles {
        let crt = if is_shared { "dllcrt2.o" } else { "crt2.o" };
        let mut restored: Vec<OsString> = Vec::with_capacity(args.len() + 5);
        restored.push(OsString::from(crt_dir.join(crt).as_os_str()));
        restored.append(&mut args);
        restored.push(OsString::from("-nostartfiles"));
        restored.push(OsString::from("-L"));
        restored.push(OsString::from(crt_dir.as_os_str()));
        args = restored;
    }

    let mut command = Command::new(&real_linker);
    command.args(&args);
    // (C) gcc needs its sibling tools (ld.exe) on PATH.
    if let Ok(path) = std::env::var("PATH") {
        command.env("PATH", format!("{};{path}", self_contained.display()));
    } else {
        command.env("PATH", &self_contained);
    }
    match command.status() {
        Ok(status) => exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("linker-shim: cannot launch real linker: {error}");
            exit(1);
        }
    }
}
