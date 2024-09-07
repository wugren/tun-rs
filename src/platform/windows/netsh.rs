use std::io;
use std::net::IpAddr;
use std::os::windows::process::CommandExt;
use std::process::{Command, Output};

use encoding_rs::GBK;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

pub fn set_interface_name(old_name: &str, new_name: &str) -> io::Result<()> {
    let cmd = format!(
        " netsh interface set interface name={:?} newname={:?}",
        old_name, new_name
    );
    exe_cmd(&cmd)
}
pub fn set_interface_metric(index: u32, metric: u16) -> io::Result<()> {
    let cmd = format!(
        "netsh interface ip set interface {} metric={}",
        index, metric
    );
    exe_cmd(&cmd)
}
pub fn exe_cmd(cmd: &str) -> io::Result<()> {
    let out = Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .arg("/C")
        .arg(cmd)
        .output()?;
    output(cmd, out)
}
fn gbk_to_utf8(bytes: &[u8]) -> String {
    let (msg, _, _) = GBK.decode(bytes);
    msg.to_string()
}
fn output(cmd: &str, out: Output) -> io::Result<()> {
    if !out.status.success() {
        let msg = if !out.stderr.is_empty() {
            match std::str::from_utf8(&out.stderr) {
                Ok(msg) => msg.to_string(),
                Err(_) => gbk_to_utf8(&out.stderr),
            }
        } else if !out.stdout.is_empty() {
            match std::str::from_utf8(&out.stdout) {
                Ok(msg) => msg.to_string(),
                Err(_) => gbk_to_utf8(&out.stdout),
            }
        } else {
            String::new()
        };
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("cmd={:?},out={:?}", cmd, msg),
        ));
    }
    Ok(())
}
pub fn exe_command(cmd: &mut Command) -> io::Result<()> {
    let out = cmd.creation_flags(CREATE_NO_WINDOW).output()?;
    output(&format!("{:?}", cmd), out)
}

/// 设置网卡ip
pub fn set_interface_ip(
    index: u32,
    address: IpAddr,
    netmask: IpAddr,
    gateway: Option<IpAddr>,
) -> io::Result<()> {
    let mut binding = Command::new("netsh");
    let cmd = binding
        .arg("interface")
        .arg(if address.is_ipv4() { "ipv4" } else { "ipv6" })
        .arg("set")
        .arg("address")
        .arg(index.to_string().as_str())
        .arg("source=static")
        .arg(format!("address={}", address).as_str())
        .arg(format!("mask={}", netmask).as_str());
    if let Some(gateway) = gateway {
        _ = cmd.arg(format!("gateway={}", gateway).as_str());
    }
    exe_command(cmd)
}

pub fn set_interface_mtu(index: u32, mtu: u32) -> io::Result<()> {
    let cmd = format!(
        "netsh interface ipv4 set subinterface {}  mtu={} store=persistent",
        index, mtu
    );
    exe_cmd(&cmd)
}
