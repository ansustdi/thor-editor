//! Oracle client discovery: installed homes, bundled Instant Client, tnsnames.ora aliases.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OracleHome {
    pub name: String,
    /// Directory containing oci.dll / libclntsh.
    pub lib_dir: String,
    /// Directory holding tnsnames.ora, if found.
    pub config_dir: Option<String>,
    pub bundled: bool,
}

static BUNDLED_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Called at startup with the app resource dir; looks for `instantclient/`.
/// `\\?\C:\x` → `C:\x`. Tauri returns verbatim paths; OCI loads oci.dll from them but then fails
/// on its own files (shows up as ORA-28041), so plain paths are required.
pub fn plain_path(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p
    }
}

pub fn set_resource_dir(dir: Option<PathBuf>) {
    let found = dir.map(|d| plain_path(d).join("instantclient")).filter(|d| has_oci(d));
    let _ = BUNDLED_DIR.set(found);
}

pub fn bundled_dir() -> Option<PathBuf> {
    BUNDLED_DIR.get().cloned().flatten()
}

fn has_oci(dir: &Path) -> bool {
    if cfg!(windows) {
        return dll_matches_arch(&dir.join("oci.dll"));
    }
    ["libclntsh.so", "libclntsh.dylib"].iter().any(|f| dir.join(f).exists())
}

/// A 64-bit process cannot load a 32-bit oci.dll (and vice versa); check the PE machine type.
fn dll_matches_arch(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut buf = [0u8; 4];
    let pe_offset = f
        .seek(SeekFrom::Start(0x3C))
        .and_then(|_| f.read_exact(&mut buf))
        .map(|_| u32::from_le_bytes(buf) as u64);
    let Ok(off) = pe_offset else { return false };
    let mut mach = [0u8; 2];
    if f.seek(SeekFrom::Start(off + 4)).and_then(|_| f.read_exact(&mut mach)).is_err() {
        return false;
    }
    let machine = u16::from_le_bytes(mach);
    if cfg!(target_pointer_width = "64") {
        matches!(machine, 0x8664 | 0xAA64)
    } else {
        machine == 0x14C
    }
}

/// tnsnames.ora location for a client lib dir (ORACLE_HOME\bin or Instant Client dir).
pub fn config_dir_for(lib_dir: &Path) -> Option<PathBuf> {
    if let Ok(t) = std::env::var("TNS_ADMIN") {
        let p = PathBuf::from(t);
        if p.is_dir() {
            return Some(p);
        }
    }
    let home = if lib_dir.file_name().map(|n| n.eq_ignore_ascii_case("bin")).unwrap_or(false) {
        lib_dir.parent()?.to_path_buf()
    } else {
        lib_dir.to_path_buf()
    };
    let admin = home.join("network").join("admin");
    if admin.join("tnsnames.ora").is_file() {
        return Some(admin);
    }
    // The bundled Instant Client ships no tnsnames.ora: reuse one from an installed Oracle home
    // (Toad / SQL Developer users already have their aliases there).
    installed_tns_admin().or_else(|| admin.is_dir().then_some(admin))
}

/// First network/admin directory with a tnsnames.ora among installed Oracle homes.
pub fn installed_tns_admin() -> Option<PathBuf> {
    let mut homes: Vec<PathBuf> = vec![];
    #[cfg(windows)]
    for (_, home) in registry_homes() {
        homes.push(PathBuf::from(home));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.join("oci.dll").exists() || dir.join("libclntsh.so").exists() {
                homes.push(if dir.file_name().map(|n| n.eq_ignore_ascii_case("bin")).unwrap_or(false) {
                    dir.parent().map(Path::to_path_buf).unwrap_or(dir)
                } else {
                    dir
                });
            }
        }
    }
    homes
        .into_iter()
        .map(|h| h.join("network").join("admin"))
        .find(|a| a.join("tnsnames.ora").is_file())
}

pub fn list_homes() -> Vec<OracleHome> {
    let mut out: Vec<OracleHome> = vec![];
    let mut push = |name: String, lib: PathBuf, bundled: bool| {
        let lib_s = lib.to_string_lossy().to_string();
        if out.iter().any(|h| h.lib_dir.eq_ignore_ascii_case(&lib_s)) {
            return;
        }
        out.push(OracleHome {
            name,
            config_dir: config_dir_for(&lib).map(|p| p.to_string_lossy().to_string()),
            lib_dir: lib_s,
            bundled,
        });
    };

    if let Some(b) = bundled_dir() {
        push("Bundled Instant Client".into(), b, true);
    }

    #[cfg(windows)]
    for (name, home) in registry_homes() {
        let bin = PathBuf::from(&home).join("bin");
        if has_oci(&bin) {
            push(name, bin, false);
        } else if has_oci(Path::new(&home)) {
            push(name, PathBuf::from(home), false);
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            if has_oci(&dir) {
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| dir.to_string_lossy().to_string());
                push(name, dir, false);
            }
        }
    }
    out
}

#[cfg(windows)]
fn registry_homes() -> Vec<(String, String)> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY};
    use winreg::RegKey;
    let mut res = vec![];
    for view in [KEY_WOW64_64KEY, KEY_WOW64_32KEY] {
        let Ok(oracle) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags("SOFTWARE\\ORACLE", KEY_READ | view)
        else {
            continue;
        };
        for key in oracle.enum_keys().flatten() {
            if !key.to_ascii_uppercase().starts_with("KEY_") {
                continue;
            }
            if let Ok(k) = oracle.open_subkey_with_flags(&key, KEY_READ | view) {
                let home: Result<String, _> = k.get_value("ORACLE_HOME");
                let name: String = k.get_value("ORACLE_HOME_NAME").unwrap_or_else(|_| key[4..].to_string());
                if let Ok(home) = home {
                    res.push((name, home));
                }
            }
        }
    }
    res
}

/// Top-level alias names from a tnsnames.ora file (handles `A, B = (...)` and IFILE-free files).
pub fn parse_tns_aliases(text: &str) -> Vec<String> {
    let mut aliases = vec![];
    let mut depth = 0i32;
    let mut token = String::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        for ch in line.chars() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                '=' if depth == 0 => {
                    for a in token.split(',') {
                        let a = a.trim();
                        if !a.is_empty() {
                            aliases.push(a.to_string());
                        }
                    }
                    token.clear();
                }
                c if depth == 0 => token.push(c),
                _ => {}
            }
        }
        if depth == 0 {
            token.push(' ');
        }
    }
    aliases.sort_by_key(|a| a.to_ascii_uppercase());
    aliases.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    aliases
}

pub fn tns_aliases(config_dir: &str) -> Vec<String> {
    std::fs::read_to_string(Path::new(config_dir).join("tnsnames.ora"))
        .map(|t| parse_tns_aliases(&t))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_verbatim_prefix() {
        assert_eq!(plain_path(PathBuf::from(r"\\?\C:\App\ic")), PathBuf::from(r"C:\App\ic"));
        assert_eq!(plain_path(PathBuf::from(r"\\?\UNC\srv\share\ic")), PathBuf::from(r"\\srv\share\ic"));
        assert_eq!(plain_path(PathBuf::from(r"C:\x")), PathBuf::from(r"C:\x"));
    }

    #[test]
    fn parses_aliases() {
        let t = "# comment\nPROD =\n (DESCRIPTION = (ADDRESS = (PROTOCOL = TCP)(HOST = h)(PORT = 1521))\n  (CONNECT_DATA = (SERVICE_NAME = prod)))\n\nDEV, DEV2 = (DESCRIPTION=(ADDRESS=(HOST=x)))\n";
        assert_eq!(parse_tns_aliases(t), vec!["DEV", "DEV2", "PROD"]);
    }
}

