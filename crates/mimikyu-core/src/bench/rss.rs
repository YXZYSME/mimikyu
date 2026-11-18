use std::process::Command;

pub fn measure_process_tree_rss_bytes(pid: u32) -> Option<u64> {
    if let Some(bytes) = measure_pgid_rss_via_ps(pid)
        && bytes > 0
    {
        return Some(bytes);
    }
    if let Some(bytes) = measure_single_pid_via_ps(pid)
        && bytes > 0
    {
        return Some(bytes);
    }
    #[cfg(target_os = "linux")]
    if let Some(bytes) = measure_pgid_rss_via_proc(pid)
        && bytes > 0
    {
        return Some(bytes);
    }
    None
}

fn measure_pgid_rss_via_ps(pgid: u32) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-axo", "pgid=,rss="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let target = i64::from(pgid);
    let mut total_kb: u64 = 0;
    let mut matched = false;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(pg_str) = parts.next() else {
            continue;
        };
        let Some(rss_str) = parts.next() else {
            continue;
        };
        if let (Ok(pg), Ok(rss_kb)) = (pg_str.parse::<i64>(), rss_str.parse::<u64>())
            && pg == target
        {
            matched = true;
            total_kb = total_kb.saturating_add(rss_kb);
        }
    }
    if matched && total_kb > 0 {
        Some(total_kb.saturating_mul(1024))
    } else {
        None
    }
}

fn measure_single_pid_via_ps(pid: u32) -> Option<u64> {
    let pid_str = pid.to_string();
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid_str])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut total_kb: u64 = 0;
    for line in text.lines() {
        if let Ok(kb) = line.trim().parse::<u64>() {
            total_kb = total_kb.saturating_add(kb);
        }
    }
    if total_kb > 0 {
        Some(total_kb.saturating_mul(1024))
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn measure_pgid_rss_via_proc(pgid: u32) -> Option<u64> {
    use std::fs;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    let page_bytes = page_size as u64;
    let entries = fs::read_dir("/proc").ok()?;
    let target = pgid.to_string();
    let mut total_pages: u64 = 0;
    let mut matched = false;

    for entry in entries.flatten() {
        let fname = entry.file_name();
        let pid_str = fname.to_string_lossy();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let stat_path = entry.path().join("stat");
        let Ok(stat) = fs::read_to_string(&stat_path) else {
            continue;
        };
        let Some(rparen) = stat.rfind(')') else {
            continue;
        };
        let after = stat.get(rparen + 2..)?;
        let mut fields = after.split_whitespace();
        let _state = fields.next();
        let _ppid = fields.next();
        let Some(pgrp) = fields.next() else {
            continue;
        };
        if pgrp != target && pid_str != target {
            continue;
        }
        let statm_path = entry.path().join("statm");
        if let Ok(statm) = fs::read_to_string(&statm_path)
            && let Some(res_pages) = statm
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u64>().ok())
        {
            matched = true;
            total_pages = total_pages.saturating_add(res_pages);
        }
    }

    if matched && total_pages > 0 {
        Some(total_pages.saturating_mul(page_bytes))
    } else {
        None
    }
}

pub fn measure_self_rss_bytes() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return 1;
    }
    let usage = unsafe { usage.assume_init() };
    let raw = usage.ru_maxrss;
    if raw <= 0 {
        return 1;
    }
    #[cfg(target_os = "macos")]
    {
        raw as u64
    }
    #[cfg(not(target_os = "macos"))]
    {
        (raw as u64).saturating_mul(1024)
    }
}
