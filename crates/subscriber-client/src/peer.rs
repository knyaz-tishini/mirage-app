//! Which local process opened this loopback socket.
//!
//! SOCKS and HTTP are opt-in: the app connects to us. The scope's app list
//! is that process, not a second tunnel. Unknown process + a non-empty app
//! list is a refusal, decided by the caller.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::net::TcpStream;

/// Full path of the process at the other end of `stream`. `None` if it
/// cannot be named.
pub fn owning_exe(stream: &TcpStream) -> Option<String> {
    let local = stream.local_addr().ok()?;
    let remote = stream.peer_addr().ok()?;
    owning_exe_of(local, remote)
}

fn owning_exe_of(local: SocketAddr, remote: SocketAddr) -> Option<String> {
    let (IpAddr::V4(local_ip), IpAddr::V4(remote_ip)) = (local.ip(), remote.ip()) else {
        return None;
    };
    #[cfg(windows)]
    {
        return windows_exe(local_ip, local.port(), remote_ip, remote.port());
    }
    #[cfg(target_os = "linux")]
    {
        return linux_exe(local_ip, local.port(), remote_ip, remote.port());
    }
    #[cfg(target_os = "macos")]
    {
        return macos_exe(local.port(), remote.port());
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = (local_ip, remote_ip);
        None
    }
}

#[cfg(windows)]
fn windows_exe(local_ip: Ipv4Addr, local_port: u16, remote_ip: Ipv4Addr, remote_port: u16) -> Option<String> {
    // The accepted socket belongs to this process. The connector is the same
    // 4-tuple with the ends swapped, and that row owns the app we filter on.
    let pid = windows_owner_pid(remote_ip, remote_port, local_ip, local_port)
        .or_else(|| windows_owner_pid(local_ip, local_port, remote_ip, remote_port))?;
    windows_pid_path(pid)
}

#[cfg(windows)]
fn windows_owner_pid(local_ip: Ipv4Addr, local_port: u16, remote_ip: Ipv4Addr, remote_port: u16) -> Option<u32> {
    use std::ptr;
    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetExtendedTcpTable(
            table: *mut u8,
            size: *mut u32,
            order: i32,
            af: u32,
            class: i32,
            reserved: u32,
        ) -> u32;
    }
    const AF_INET: u32 = 2;
    const TCP_TABLE_OWNER_PID_ALL: i32 = 5;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    let mut size = 0u32;
    let rc = unsafe {
        GetExtendedTcpTable(ptr::null_mut(), &mut size, 0, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0)
    };
    if rc != ERROR_INSUFFICIENT_BUFFER || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let rc = unsafe {
        GetExtendedTcpTable(buf.as_mut_ptr(), &mut size, 0, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0)
    };
    if rc != 0 || size < 4 {
        return None;
    }
    let n = u32::from_le_bytes(buf[0..4].try_into().ok()?) as usize;
    // MIB_TCPROW_OWNER_PID is 6 dwords. dwLocalPort / dwRemotePort are network order.
    let row = 24usize;
    let local_addr = u32::from(local_ip).to_be();
    let remote_addr = u32::from(remote_ip).to_be();
    for i in 0..n {
        let off = 4 + i * row;
        if off + row > buf.len() {
            break;
        }
        let rec = &buf[off..off + row];
        let row_local_addr = u32::from_le_bytes(rec[4..8].try_into().ok()?);
        let row_local_port = u16::from_be_bytes([rec[8], rec[9]]);
        let row_remote_addr = u32::from_le_bytes(rec[12..16].try_into().ok()?);
        let row_remote_port = u16::from_be_bytes([rec[16], rec[17]]);
        let pid = u32::from_le_bytes(rec[20..24].try_into().ok()?);
        if row_local_addr == local_addr
            && row_local_port == local_port
            && row_remote_addr == remote_addr
            && row_remote_port == remote_port
        {
            return Some(pid);
        }
    }
    None
}

#[cfg(windows)]
fn windows_pid_path(pid: u32) -> Option<String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut u8;
        fn CloseHandle(handle: *mut u8) -> i32;
        fn QueryFullProcessImageNameW(process: *mut u8, flags: u32, name: *mut u16, size: *mut u32) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

#[cfg(target_os = "linux")]
fn linux_exe(local_ip: Ipv4Addr, local_port: u16, remote_ip: Ipv4Addr, remote_port: u16) -> Option<String> {
    let inode = linux_inode(remote_ip, remote_port, local_ip, local_port)
        .or_else(|| linux_inode(local_ip, local_port, remote_ip, remote_port))?;
    let needle = format!("socket:[{inode}]");
    let proc = std::fs::read_dir("/proc").ok()?;
    for entry in proc.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let fd_dir = entry.path().join("fd");
        let fds = match std::fs::read_dir(&fd_dir) {
            Ok(fds) => fds,
            Err(_) => continue,
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else { continue };
            if target.to_string_lossy() == needle {
                let exe = std::fs::read_link(entry.path().join("exe")).ok()?;
                return Some(exe.to_string_lossy().into_owned());
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn linux_inode(local_ip: Ipv4Addr, local_port: u16, remote_ip: Ipv4Addr, remote_port: u16) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/net/tcp").ok()?;
    let want_local = format!("{:08X}:{:04X}", u32::from(local_ip).swap_bytes(), local_port);
    let want_remote = format!("{:08X}:{:04X}", u32::from(remote_ip).swap_bytes(), remote_port);
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 {
            continue;
        }
        if fields[1].eq_ignore_ascii_case(&want_local) && fields[2].eq_ignore_ascii_case(&want_remote) {
            return fields[9].parse().ok();
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn macos_exe(local_port: u16, remote_port: u16) -> Option<String> {
    // The accepted socket is ours. The connector's local port is `remote_port`,
    // and lsof names that end `:<remote_port>->…:<local_port>`.
    let pid = macos_lsof_pid(remote_port, local_port).or_else(|| macos_lsof_pid(local_port, remote_port))?;
    macos_pid_path(pid)
}

#[cfg(target_os = "macos")]
fn macos_lsof_pid(local_side: u16, remote_side: u16) -> Option<u32> {
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-F", "pn", "-i", &format!("TCP:{local_side}"), "-sTCP:ESTABLISHED"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut pid: Option<u32> = None;
    let local_mark = format!(":{local_side}->");
    let remote_mark = format!(":{remote_side}");
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            pid = rest.parse().ok();
        } else if let Some(name) = line.strip_prefix('n') {
            if name.contains(&local_mark) && name.contains(&remote_mark) {
                return pid;
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn macos_pid_path(pid: u32) -> Option<String> {
    let out = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "txt", "-F", "n"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(path) = line.strip_prefix('n') {
            if path.starts_with('/') {
                return Some(path.to_string());
            }
        }
    }
    None
}
