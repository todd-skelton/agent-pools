//! The jobs Agent Pools ran through wscript.exe and Windows PowerShell, as commands of this
//! program, which the scheduled tasks start directly:
//!
//! | Command | Agent Pools |
//! |---|---|
//! | `tunnel <root>` | Run-Cloudflare-Tunnel.vbs: `cloudflared tunnel run` with `state/cloudflared/config.yml`, hidden, waiting while one for that config is already running |
//! | `backup <root> [--destination <dir>] [--keep <n>]` | Backup-Pool-Secrets.ps1: a zip of the credentials and state, protected with DPAPI for this Windows account, the newest `keep` kept |
//! | `restore [<backup>] [--target <dir>] [--list]` | Restore-Pool-Secrets.ps1: a backup opened into a directory apart from the install, or its entries listed |
//!
//! A backup is the same file Backup-Pool-Secrets.ps1 writes: a standard zip (deflate), protected
//! with `CryptProtectData` in the current user's scope, so either side restores the other's.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Backup-Pool-Secrets.ps1's defaults.
pub const BACKUPS: &str = r"D:\Users\ToddS\AgentPools-backups";
pub const KEEP: usize = 14;
/// What a backup holds, under the install (each when it is there; `auth` and `proxy.yaml` must be).
const BACKED_UP: [&str; 4] = ["state/secrets.json", "state/compaction-keys.json", "state/install.json", "state/pool-state.json"];

fn other(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// The value after `flag` in `args`.
fn flag<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(String::as_str)
}

/// The arguments that are not flags or flag values.
fn positional(args: &[String], flags: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
        } else if flags.contains(&a.as_str()) {
            skip = true;
        } else if !a.starts_with("--") {
            out.push(a.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The tunnel

/// `tunnel <root>`: cloudflared for the install's tunnel, as Run-Cloudflare-Tunnel.vbs runs it.
/// Exits 2 when cloudflared or the config is missing; otherwise waits while a cloudflared for
/// this config is running (stopping the task ends this program, not its cloudflared, so a
/// keepalive would add a second connector), then runs one hidden and exits with its code.
pub async fn tunnel(args: &[String]) -> std::io::Result<i32> {
    let root = positional(args, &[]).into_iter().next().ok_or_else(|| other("usage: pools-rs tunnel <root>"))?;
    let root = PathBuf::from(root);
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).ok_or_else(|| other("LOCALAPPDATA is not set"))?;
    let exe = local.join(r"Programs\cloudflared\cloudflared.exe");
    let config = root.join(r"state\cloudflared\config.yml");
    if !exe.is_file() || !config.is_file() {
        return Ok(2);
    }
    let needle = config.to_string_lossy().to_lowercase();
    loop {
        let running = super::machine::processes_named("cloudflared.exe").into_iter().any(|pid| {
            super::machine::win_process_info(pid).and_then(|i| i.get("CommandLine").and_then(|c| c.as_str()).map(|c| c.to_lowercase().contains(&needle))).unwrap_or(false)
        });
        if !running {
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.args(["--no-autoupdate", "--config"]).arg(&config).args(["tunnel", "run"]).current_dir(&root);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    super::machine::hide_window(&mut cmd);
    let status = cmd.spawn()?.wait().await?;
    Ok(status.code().unwrap_or(1))
}

// ---------------------------------------------------------------------------------------------
// Backups

/// `backup <root> [--destination <dir>] [--keep <n>]`: writes `agentpools-<stamp>.zip.dpapi`
/// into the destination and removes all but the newest `keep`. Prints the file and its size.
pub fn backup(args: &[String]) -> std::io::Result<()> {
    let root = positional(args, &["--destination", "--keep"]).into_iter().next().ok_or_else(|| other("usage: pools-rs backup <root> [--destination <dir>] [--keep <n>]"))?;
    let dest = PathBuf::from(flag(args, "--destination").unwrap_or(BACKUPS));
    let keep = flag(args, "--keep").map(str::parse).transpose().map_err(other)?.unwrap_or(KEEP);
    let out = write_backup(Path::new(&root), &dest, keep, std::time::SystemTime::now())?;
    println!("Backup written: {} ({} KB)", out.display(), (std::fs::metadata(&out)?.len() as f64 / 1024.0).round());
    Ok(())
}

/// The entries a backup of `root` holds: `auth/...`, the state files that are there, `proxy.yaml`.
fn entries(root: &Path) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, Vec<u8>)>) -> std::io::Result<()> {
        let mut items: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        items.sort_by_key(|e| e.file_name());
        for e in items {
            let name = format!("{rel}/{}", e.file_name().to_string_lossy());
            if e.file_type()?.is_dir() {
                walk(&e.path(), &name, out)?;
            } else {
                out.push((name, std::fs::read(e.path())?));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(&root.join("auth"), "auth", &mut out)?;
    for rel in BACKED_UP {
        match std::fs::read(root.join(rel)) {
            Ok(b) => out.push((rel.to_string(), b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    out.push(("proxy.yaml".into(), std::fs::read(root.join("proxy.yaml"))?));
    Ok(out)
}

pub fn write_backup(root: &Path, dest: &Path, keep: usize, now: std::time::SystemTime) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dest)?;
    let ms = now.duration_since(std::time::UNIX_EPOCH).map_err(other)?.as_millis() as f64;
    let iso = pool_core::js::iso(ms);
    // yyyyMMddTHHmmssZ, as the script names them.
    let stamp: String = format!("{}Z", iso[..19].replace(['-', ':'], ""));
    let zipped = zip::write(&entries(root)?, &iso)?;
    let sealed = dpapi::protect(&zipped)?;
    let out = dest.join(format!("agentpools-{stamp}.zip.dpapi"));
    let part = dest.join(format!(".agentpools-{stamp}.partial"));
    std::fs::write(&part, &sealed)?;
    std::fs::rename(&part, &out)?;
    let mut all: Vec<PathBuf> = std::fs::read_dir(dest)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("agentpools-") && n.ends_with(".zip.dpapi")))
        .collect();
    all.sort();
    all.reverse();
    for old in all.iter().skip(keep) {
        std::fs::remove_file(old)?;
    }
    Ok(out)
}

/// `restore [<backup>] [--target <dir>] [--list]`: the newest backup in the default directory
/// unless one is named. `--list` prints each entry and its size and writes nothing; otherwise the
/// entries go into `--target` (default: `restored-<stamp>` beside the backup), which must not
/// exist yet, so nothing is overwritten and the install itself is never the target.
pub fn restore(args: &[String]) -> std::io::Result<()> {
    let named = positional(args, &["--target"]).into_iter().next();
    let backup = match named {
        Some(b) => PathBuf::from(b),
        None => {
            let mut all: Vec<PathBuf> = std::fs::read_dir(BACKUPS)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("agentpools-") && n.ends_with(".zip.dpapi")))
                .collect();
            all.sort();
            all.pop().ok_or_else(|| other("No backup archive found"))?
        }
    };
    let files = zip::read(&dpapi::unprotect(&std::fs::read(&backup)?)?)?;
    if args.iter().any(|a| a == "--list") {
        for (name, data) in &files {
            println!("{name}\t{}", data.len());
        }
        return Ok(());
    }
    let file_name = backup.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let target = match flag(args, "--target") {
        Some(t) => PathBuf::from(t),
        None => backup.with_file_name(format!("restored-{}", file_name.trim_start_matches("agentpools-").trim_end_matches(".zip.dpapi"))),
    };
    if target.exists() {
        return Err(other(format!("{} exists; restore into a new directory", target.display())));
    }
    if target.join("state").join("install.json").exists() || target.join("proxy.yaml").exists() {
        return Err(other("refusing to restore over an install"));
    }
    for (name, data) in &files {
        let p = target.join(name);
        std::fs::create_dir_all(p.parent().unwrap_or(&target))?;
        std::fs::write(p, data)?;
    }
    println!("Restored {} to {}", backup.display(), target.display());
    println!("Review the contents, then restore auth/, state/secrets.json, state/compaction-keys.json (when present), state/install.json, state/pool-state.json and proxy.yaml into the pool root.");
    Ok(())
}

/// The data protection API in the current user's scope, as .NET's `ProtectedData` calls it.
pub mod dpapi {
    #[cfg(windows)]
    fn call(data: &[u8], protect: bool) -> std::io::Result<Vec<u8>> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData};
        let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(data.len()).map_err(super::other)?, pbData: data.as_ptr().cast_mut() };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: `input` borrows `data` for the call (DPAPI does not write through it); `out` is
        // allocated by DPAPI, copied, then freed once with LocalFree.
        unsafe {
            let ok = if protect {
                CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData.cast());
            Ok(bytes)
        }
    }

    #[cfg(not(windows))]
    fn call(_: &[u8], _: bool) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other("DPAPI is Windows only"))
    }

    pub fn protect(data: &[u8]) -> std::io::Result<Vec<u8>> {
        call(data, true)
    }

    pub fn unprotect(data: &[u8]) -> std::io::Result<Vec<u8>> {
        call(data, false)
    }
}

/// A plain zip: deflated entries, a central directory, no zip64 (backups are kilobytes).
pub mod zip {
    use std::io::Write;

    fn le16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn le32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn crc(data: &[u8]) -> u32 {
        let mut c = flate2::Crc::new();
        c.update(data);
        c.sum()
    }

    /// The DOS time and date of an ISO UTC time (`yyyy-mm-ddThh:mm:ss`).
    fn dos(iso: &str) -> (u16, u16) {
        let n = |a: usize, b: usize| iso.get(a..b).and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
        let (y, mo, d, h, mi, s) = (n(0, 4), n(5, 7), n(8, 10), n(11, 13), n(14, 16), n(17, 19));
        ((h << 11) | (mi << 5) | (s / 2), ((y.saturating_sub(1980)) << 9) | (mo << 5) | d)
    }

    pub fn write(files: &[(String, Vec<u8>)], iso: &str) -> std::io::Result<Vec<u8>> {
        let (time, date) = dos(iso);
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
            enc.write_all(data)?;
            let packed = enc.finish()?;
            let (sum, offset) = (crc(data), u32::try_from(out.len()).map_err(super::other)?);
            let (csize, usize_) = (u32::try_from(packed.len()).map_err(super::other)?, u32::try_from(data.len()).map_err(super::other)?);
            let name_len = u16::try_from(name.len()).map_err(super::other)?;
            // Bit 11: the name is UTF-8.
            let head = |v: &mut Vec<u8>| {
                le16(v, 0x0800);
                le16(v, 8);
                le16(v, time);
                le16(v, date);
                le32(v, sum);
                le32(v, csize);
                le32(v, usize_);
                le16(v, name_len);
                le16(v, 0);
            };
            le32(&mut out, 0x0403_4b50);
            le16(&mut out, 20);
            head(&mut out);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&packed);
            le32(&mut central, 0x0201_4b50);
            le16(&mut central, 20);
            le16(&mut central, 20);
            head(&mut central);
            le16(&mut central, 0); // comment
            le16(&mut central, 0); // disk
            le16(&mut central, 0); // internal attributes
            le32(&mut central, 0); // external attributes
            le32(&mut central, offset);
            central.extend_from_slice(name.as_bytes());
        }
        let (at, count) = (u32::try_from(out.len()).map_err(super::other)?, u16::try_from(files.len()).map_err(super::other)?);
        let size = u32::try_from(central.len()).map_err(super::other)?;
        out.extend_from_slice(&central);
        le32(&mut out, 0x0605_4b50);
        le16(&mut out, 0);
        le16(&mut out, 0);
        le16(&mut out, count);
        le16(&mut out, count);
        le32(&mut out, size);
        le32(&mut out, at);
        le16(&mut out, 0);
        Ok(out)
    }

    /// The files in a zip (stored or deflated), each checked against its CRC, with names that
    /// stay inside the directory they are opened into (`/` or `\\`, no `..`, not absolute).
    pub fn read(zip: &[u8]) -> std::io::Result<Vec<(String, Vec<u8>)>> {
        use std::io::Read;
        let bad = |what: &str| std::io::Error::other(format!("not a backup zip: {what}"));
        let u16_at = |i: usize| zip.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).ok_or_else(|| bad("truncated"));
        let u32_at = |i: usize| zip.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or_else(|| bad("truncated"));
        let end = (0..zip.len().saturating_sub(21)).rev().take(65_557).find(|&i| zip[i..].starts_with(&[0x50, 0x4b, 0x05, 0x06])).ok_or_else(|| bad("no end record"))?;
        let (count, mut at) = (u16_at(end + 10)?, u32_at(end + 16)?);
        let mut files = Vec::with_capacity(count);
        for _ in 0..count {
            if u32_at(at)? != 0x0201_4b50 {
                return Err(bad("central directory"));
            }
            let (method, sum, csize, size) = (u16_at(at + 10)?, u32_at(at + 16)? as u32, u32_at(at + 20)?, u32_at(at + 24)?);
            let (nlen, xlen, clen, local) = (u16_at(at + 28)?, u16_at(at + 30)?, u16_at(at + 32)?, u32_at(at + 42)?);
            let name = String::from_utf8_lossy(zip.get(at + 46..at + 46 + nlen).ok_or_else(|| bad("name"))?).replace('\\', "/");
            at += 46 + nlen + xlen + clen;
            if name.ends_with('/') {
                continue;
            }
            if name.starts_with('/') || name.contains(':') || name.split('/').any(|p| p == ".." || p.is_empty()) {
                return Err(bad(&format!("unsafe name {name}")));
            }
            let start = local + 30 + u16_at(local + 26)? + u16_at(local + 28)?;
            let packed = zip.get(start..start + csize).ok_or_else(|| bad("data"))?;
            let data = match method {
                0 => packed.to_vec(),
                8 => {
                    let mut v = Vec::with_capacity(size);
                    flate2::read::DeflateDecoder::new(packed).read_to_end(&mut v)?;
                    v
                }
                m => return Err(bad(&format!("method {m}"))),
            };
            if data.len() != size || crc(&data) != sum {
                return Err(bad(&format!("{name} does not match its checksum")));
            }
            files.push((name, data));
        }
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pools-ops-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_zip_reads_back_as_written_and_refuses_names_outside_its_directory() {
        let files = vec![("auth/a.json".to_string(), b"{\"a\":1}".to_vec()), ("proxy.yaml".to_string(), vec![b'x'; 10_000]), ("state/empty.json".into(), vec![])];
        let z = zip::write(&files, "2026-10-28T08:30:04.000Z").unwrap();
        assert_eq!(zip::read(&z).unwrap(), files);
        let evil = zip::write(&[("../x".into(), b"1".to_vec())], "2026-10-28T08:30:04.000Z").unwrap();
        assert!(zip::read(&evil).is_err());
        let mut flipped = z.clone();
        let i = flipped.windows(4).position(|w| w == b"\x50\x4b\x03\x04").unwrap() + 30 + "auth/a.json".len() + 2;
        flipped[i] ^= 0xff;
        assert!(zip::read(&flipped).is_err(), "a changed byte fails its checksum");
    }

    #[cfg(windows)]
    #[test]
    fn a_backup_holds_the_install_s_credentials_and_keeps_the_newest() {
        let root = scratch("root");
        std::fs::create_dir_all(root.join("auth/nested")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::write(root.join("auth/one.json"), "made up 1").unwrap();
        std::fs::write(root.join("auth/nested/two.json"), "made up 2").unwrap();
        std::fs::write(root.join("state/secrets.json"), "{\"managementKey\":\"made-up\"}").unwrap();
        std::fs::write(root.join("state/install.json"), "{}").unwrap();
        std::fs::write(root.join("proxy.yaml"), "port: 1\n").unwrap();
        let dest = scratch("dest");
        let t0 = std::time::UNIX_EPOCH + Duration::from_secs(1_793_000_000);
        let mut made = Vec::new();
        for i in 0..4 {
            made.push(write_backup(&root, &dest, 3, t0 + Duration::from_secs(i * 60)).unwrap());
        }
        assert!(!made[0].exists() && made[1..].iter().all(|p| p.exists()), "the newest three are kept");
        assert!(made[3].file_name().unwrap().to_str().unwrap().starts_with("agentpools-2026"), "{}", made[3].display());
        let sealed = std::fs::read(&made[3]).unwrap();
        assert!(zip::read(&sealed).is_err(), "the file is protected, not a plain zip");
        let names: Vec<String> = zip::read(&dpapi::unprotect(&sealed).unwrap()).unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["auth/nested/two.json", "auth/one.json", "state/secrets.json", "state/install.json", "proxy.yaml"]);
        let target = dest.join("restored");
        restore(&[made[3].to_string_lossy().into(), "--target".into(), target.to_string_lossy().into()]).unwrap();
        assert_eq!(std::fs::read_to_string(target.join("auth/nested/two.json")).unwrap(), "made up 2");
        assert!(restore(&[made[3].to_string_lossy().into(), "--target".into(), target.to_string_lossy().into()]).is_err(), "never over an existing directory");
    }
}
