use crate::{
    configuration::{Configuration, Layer},
    device::ETHER_ADDR_LEN,
    platform::freebsd::sys::*,
    platform::posix::{self, sockaddr_union, Fd, Tun},
    ToIpv4Netmask, ToIpv6Netmask,
};

use crate::platform::posix::device::{ctl, ctl_v6};
use libc::{
    self, c_char, c_short, fcntl, ifreq, kinfo_file, AF_LINK, F_KINFO, IFF_RUNNING, IFF_UP,
    IFNAMSIZ, KINFO_FILE_SIZE, O_RDWR,
};
use mac_address::mac_address_by_name;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::{ffi::CStr, io, mem, net::IpAddr, os::unix::io::AsRawFd, ptr, sync::Mutex};

#[derive(Clone, Copy, Debug)]
struct Route {
    addr: IpAddr,
    netmask: IpAddr,
    #[allow(dead_code)]
    dest: IpAddr,
}

/// A TUN device using the TUN/TAP Linux driver.
pub struct Device {
    pub(crate) tun: Tun,
    alias_lock: Mutex<()>,
}

impl Device {
    /// Create a new `Device` for the given `Configuration`.
    pub fn new(config: Configuration) -> std::io::Result<Self> {
        let layer = config.layer.unwrap_or(Layer::L3);
        let device_prefix = if layer == Layer::L3 {
            "tun".to_string()
        } else {
            "tap".to_string()
        };
        let device = unsafe {
            let dev_index = match config.dev_name.as_ref() {
                Some(tun_name) => {
                    let tun_name = tun_name.clone();

                    if tun_name.len() > IFNAMSIZ {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "device name too long",
                        ));
                    }

                    if layer == Layer::L3 && !tun_name.starts_with("tun") {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "device name must start with tun",
                        ));
                    }
                    if layer == Layer::L2 && !tun_name.starts_with("tap") {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "device name must start with tap",
                        ));
                    }
                    Some(
                        tun_name[3..]
                            .parse::<u32>()
                            .map_err(|e| std::io::Error::new(ErrorKind::InvalidInput, e))?
                            + 1_u32,
                    )
                }

                None => None,
            };

            let (tun, _tun_name) = {
                if let Some(name_index) = dev_index.as_ref() {
                    let device_name = format!("{}{}", device_prefix, name_index);
                    let device_path = format!("/dev/{}\0", device_name);
                    let fd = libc::open(device_path.as_ptr() as *const _, O_RDWR);
                    let tun = Fd::new(fd).map_err(|_| io::Error::last_os_error())?;
                    (tun, device_name)
                } else {
                    let (tun, device_name) = 'End: {
                        for i in 0..256 {
                            let device_name = format!("{device_prefix}{i}");
                            let device_path = format!("/dev/{device_name}\0");
                            let fd = libc::open(device_path.as_ptr() as *const _, O_RDWR);
                            if fd > 0 {
                                let tun = Fd::new(fd).map_err(|_| io::Error::last_os_error())?;
                                break 'End (tun, device_name);
                            }
                        }
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            "no available file descriptor",
                        ));
                    };
                    (tun, device_name)
                }
            };

            Device {
                tun: Tun::new(tun),
                alias_lock: Mutex::new(()),
            }
        };
        config.config(&device)?;

        Ok(device)
    }
    pub fn from_tun(tun: Tun) -> Self {
        Self {
            tun,
            alias_lock: Mutex::new(()),
        }
    }
    // fn current_route(&self) -> Option<Route> {
    //     let addr = self.address().ok()?;
    //     let netmask = self.netmask().ok()?;
    //     let dest = self
    //         .destination()
    //         .unwrap_or(self.calc_dest_addr(addr, netmask).ok()?);
    //     Some(Route {
    //         addr,
    //         netmask,
    //         dest,
    //     })
    // }

    fn calc_dest_addr(&self, addr: IpAddr, netmask: IpAddr) -> std::io::Result<IpAddr> {
        let prefix_len = ipnet::ip_mask_to_prefix(netmask)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        Ok(ipnet::IpNet::new(addr, prefix_len)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
            .broadcast())
    }

    /// Set the IPv4 alias of the device.
    fn set_alias(&self, addr: IpAddr, dest: IpAddr, mask: IpAddr) -> std::io::Result<()> {
        let _guard = self.alias_lock.lock().unwrap();
        // let old_route = self.current_route();
        unsafe {
            match addr {
                IpAddr::V4(_) => {
                    let ctl = ctl()?;
                    let mut req: ifaliasreq = mem::zeroed();
                    let tun_name = self.name()?;
                    ptr::copy_nonoverlapping(
                        tun_name.as_ptr() as *const c_char,
                        req.ifran.as_mut_ptr(),
                        tun_name.len(),
                    );

                    req.addr = posix::sockaddr_union::from((addr, 0)).addr;
                    req.dstaddr = posix::sockaddr_union::from((dest, 0)).addr;
                    req.mask = posix::sockaddr_union::from((mask, 0)).addr;

                    if let Err(err) = siocaifaddr(ctl.as_raw_fd(), &req) {
                        return Err(io::Error::from(err));
                    }
                }
                IpAddr::V6(_) => {
                    let IpAddr::V6(_) = mask else {
                        return Err(std::io::Error::from(ErrorKind::InvalidInput));
                    };
                    let tun_name = self.name()?;
                    let mut req: in6_ifaliasreq = mem::zeroed();
                    ptr::copy_nonoverlapping(
                        tun_name.as_ptr() as *const c_char,
                        req.ifra_name.as_mut_ptr(),
                        tun_name.len(),
                    );
                    req.ifra_addr = sockaddr_union::from((addr, 0)).addr6;
                    req.ifra_prefixmask = sockaddr_union::from((mask, 0)).addr6;
                    req.in6_addrlifetime.ia6t_vltime = 0xffffffff_u32;
                    req.in6_addrlifetime.ia6t_pltime = 0xffffffff_u32;
                    req.ifra_flags = IN6_IFF_NODAD;
                    if let Err(err) = siocaifaddr_in6(ctl_v6()?.as_raw_fd(), &req) {
                        return Err(io::Error::from(err));
                    }
                }
            }

            let new_route = Route {
                addr,
                netmask: mask,
                dest,
            };
            if let Err(e) = self.add_route(new_route) {
                log::warn!("{e:?}");
            }

            Ok(())
        }
    }

    /// Prepare a new request.
    unsafe fn request(&self) -> std::io::Result<ifreq> {
        let mut req: ifreq = mem::zeroed();
        let tun_name = self.name()?;
        ptr::copy_nonoverlapping(
            tun_name.as_ptr() as *const c_char,
            req.ifr_name.as_mut_ptr(),
            tun_name.len(),
        );

        Ok(req)
    }

    /// # Safety
    unsafe fn request_v6(&self) -> std::io::Result<in6_ifreq> {
        let tun_name = self.name()?;
        let mut req: in6_ifreq = mem::zeroed();
        ptr::copy_nonoverlapping(
            tun_name.as_ptr() as *const c_char,
            req.ifra_name.as_mut_ptr(),
            tun_name.len(),
        );
        req.ifr_ifru.ifru_flags = IN6_IFF_NODAD as _;
        Ok(req)
    }

    fn add_route(&self, route: Route) -> std::io::Result<()> {
        let if_name = self.name()?;
        let prefix_len = ipnet::ip_mask_to_prefix(route.netmask)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let args = [
            "-n",
            "add",
            if route.addr.is_ipv4() {
                "-net"
            } else {
                "-inet6"
            },
            &format!("{}/{}", route.addr, prefix_len),
            "-iface",
            &if_name,
        ];
        crate::run_command("route", &args)?;
        log::info!("route {}", args.join(" "));
        Ok(())
    }

    // fn set_address(&self, value: IpAddr) -> Result<()> {
    //     unsafe {
    //         let req = self.request();
    //         if let Err(err) = siocdifaddr(self.ctl.as_raw_fd(), &req) {
    //             return Err(io::Error::from(err).into());
    //         }
    //         let previous = self.current_route().ok_or(Error::InvalidConfig)?;
    //         self.set_alias(value, previous.dest, previous.netmask)?;
    //     }
    //     Ok(())
    // }

    // fn set_netmask(&self, value: IpAddr) -> Result<()> {
    //     unsafe {
    //         let req = self.request();
    //         if let Err(err) = siocdifaddr(self.ctl.as_raw_fd(), &req) {
    //             return Err(io::Error::from(err).into());
    //         }
    //         let previous = self.current_route().ok_or(Error::InvalidConfig)?;
    //         self.set_alias(previous.addr, previous.dest, value)?;
    //     }
    //     Ok(())
    // }

    // fn set_destination<A: IntoAddress>(&self, value: A) -> Result<()> {
    //     let value = value.into_address()?;
    //     unsafe {
    //         let req = self.request();
    //         if let Err(err) = siocdifaddr(self.ctl.as_raw_fd(), &req) {
    //             return Err(io::Error::from(err).into());
    //         }
    //         let previous = self.current_route().ok_or(Error::InvalidConfig)?;
    //         self.set_alias(previous.addr, value, previous.netmask)?;
    //     }
    //     Ok(())
    // }

    pub fn name(&self) -> std::io::Result<String> {
        use std::path::PathBuf;
        unsafe {
            let mut path_info: kinfo_file = std::mem::zeroed();
            path_info.kf_structsize = KINFO_FILE_SIZE;
            if fcntl(self.tun.as_raw_fd(), F_KINFO, &mut path_info as *mut _) < 0 {
                return Err(io::Error::last_os_error());
            }
            let dev_path = CStr::from_ptr(path_info.kf_path.as_ptr() as *const c_char)
                .to_string_lossy()
                .into_owned();
            let path = PathBuf::from(dev_path);
            let device_name = path
                .file_name()
                .ok_or(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid device name",
                ))?
                .to_string_lossy()
                .to_string();
            Ok(device_name)
        }
    }

    pub fn set_name(&self, value: &str) -> std::io::Result<()> {
        use std::ffi::CString;
        unsafe {
            if value.len() > IFNAMSIZ {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "device name too long",
                ));
            }
            let mut req = self.request()?;
            let tun_name = CString::new(value)?;
            let mut tun_name: Vec<i8> = tun_name
                .into_bytes_with_nul()
                .into_iter()
                .map(|c| c as i8)
                .collect::<_>();
            req.ifr_ifru.ifru_data = tun_name.as_mut_ptr();
            if let Err(err) = siocsifname(ctl()?.as_raw_fd(), &req) {
                return Err(io::Error::from(err));
            }

            Ok(())
        }
    }

    pub fn enabled(&self, value: bool) -> std::io::Result<()> {
        unsafe {
            let mut req = self.request()?;
            let ctl = ctl()?;
            if let Err(err) = siocgifflags(ctl.as_raw_fd(), &mut req) {
                return Err(io::Error::from(err));
            }

            if value {
                req.ifr_ifru.ifru_flags[0] |= (IFF_UP | IFF_RUNNING) as c_short;
            } else {
                req.ifr_ifru.ifru_flags[0] &= !(IFF_UP as c_short);
            }

            if let Err(err) = siocsifflags(ctl.as_raw_fd(), &req) {
                return Err(io::Error::from(err));
            }

            Ok(())
        }
    }

    // fn broadcast(&self) -> Result<IpAddr> {
    //     unsafe {
    //         let mut req = self.request()?;
    //         if let Err(err) = siocgifbrdaddr(ctl()?.as_raw_fd(), &mut req) {
    //             return Err(io::Error::from(err).into());
    //         }
    //         let sa = sockaddr_union::from(req.ifr_ifru.ifru_broadaddr);
    //         Ok(std::net::SocketAddr::try_from(sa)?.ip())
    //     }
    // }

    // fn set_broadcast<A: IntoAddress>(&self, value: A) -> Result<()> {
    //     let value = value.into_address()?;
    //     let IpAddr::V4(_) = value else {
    //         unimplemented!("do not support IPv6 yet")
    //     };
    //     unsafe {
    //         let ctl = ctl()?;
    //         let mut req = self.request()?;
    //         req.ifr_ifru.ifru_broadaddr = posix::sockaddr_union::from((value, 0)).addr;
    //         if let Err(err) = siocsifbrdaddr(ctl.as_raw_fd(), &req) {
    //             return Err(io::Error::from(err).into());
    //         }
    //         Ok(())
    //     }
    // }

    pub fn mtu(&self) -> std::io::Result<u16> {
        unsafe {
            let mut req = self.request()?;

            if let Err(err) = siocgifmtu(ctl()?.as_raw_fd(), &mut req) {
                return Err(io::Error::from(err));
            }

            let r: u16 = req
                .ifr_ifru
                .ifru_mtu
                .try_into()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            Ok(r)
        }
    }

    pub fn set_mtu(&self, value: u16) -> std::io::Result<()> {
        unsafe {
            let mut req = self.request()?;
            req.ifr_ifru.ifru_mtu = value as i32;

            if let Err(err) = siocsifmtu(ctl()?.as_raw_fd(), &req) {
                return Err(io::Error::from(err));
            }
            Ok(())
        }
    }

    pub fn set_network_address<Netmask: ToIpv4Netmask>(
        &self,
        address: Ipv4Addr,
        netmask: Netmask,
        destination: Option<Ipv4Addr>,
    ) -> std::io::Result<()> {
        let addr = address.into();
        let netmask = netmask.netmask().into();
        let default_dest = self.calc_dest_addr(addr, netmask)?;
        let dest = destination.map(|d| d.into()).unwrap_or(default_dest);
        self.set_alias(addr, dest, netmask)?;
        Ok(())
    }

    pub fn remove_address(&self, addr: IpAddr) -> io::Result<()> {
        unsafe {
            match addr {
                IpAddr::V4(addr) => {
                    let mut req_v4 = self.request()?;
                    req_v4.ifr_ifru.ifru_addr = sockaddr_union::from((addr, 0)).addr;
                    if let Err(err) = siocdifaddr(ctl()?.as_raw_fd(), &req_v4) {
                        return Err(io::Error::from(err));
                    }
                }
                IpAddr::V6(addr) => {
                    let mut req_v6 = self.request_v6()?;
                    req_v6.ifr_ifru.ifru_addr = sockaddr_union::from((addr, 0)).addr6;
                    if let Err(err) = siocdifaddr_in6(ctl_v6()?.as_raw_fd(), &req_v6) {
                        return Err(io::Error::from(err));
                    }
                }
            }
            Ok(())
        }
    }

    pub fn add_address_v6<Netmask: ToIpv6Netmask>(
        &self,
        addr: Ipv6Addr,
        netmask: Netmask,
    ) -> std::io::Result<()> {
        unsafe {
            let tun_name = self.name()?;
            let mut req: in6_ifaliasreq = mem::zeroed();
            ptr::copy_nonoverlapping(
                tun_name.as_ptr() as *const c_char,
                req.ifra_name.as_mut_ptr(),
                tun_name.len(),
            );
            req.ifra_addr = sockaddr_union::from((addr, 0)).addr6;
            let network_addr = ipnet::IpNet::new(addr.into(), netmask.prefix())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
            let mask = network_addr.netmask();
            req.ifra_prefixmask = sockaddr_union::from((mask, 0)).addr6;
            req.in6_addrlifetime.ia6t_vltime = 0xffffffff_u32;
            req.in6_addrlifetime.ia6t_pltime = 0xffffffff_u32;
            req.ifra_flags = IN6_IFF_NODAD;
            if let Err(err) = siocaifaddr_in6(ctl_v6()?.as_raw_fd(), &req) {
                return Err(io::Error::from(err));
            }
            let Ok(dest) = self.calc_dest_addr(addr.into(), mask) else {
                return Ok(());
            };
            if let Err(e) = self.add_route(Route {
                addr: addr.into(),
                netmask: mask,
                dest,
            }) {
                log::warn!("{e:?}");
            }
        }
        Ok(())
    }

    pub fn set_mac_address(&self, eth_addr: [u8; ETHER_ADDR_LEN as usize]) -> std::io::Result<()> {
        unsafe {
            let mut req = self.request()?;
            req.ifr_ifru.ifru_addr.sa_len = ETHER_ADDR_LEN;
            req.ifr_ifru.ifru_addr.sa_family = AF_LINK as u8;
            req.ifr_ifru.ifru_addr.sa_data[0..ETHER_ADDR_LEN as usize]
                .copy_from_slice(eth_addr.map(|c| c as i8).as_slice());
            if let Err(err) = siocsiflladdr(ctl()?.as_raw_fd(), &req) {
                return Err(io::Error::from(err));
            }
            Ok(())
        }
    }

    pub fn mac_address(&self) -> std::io::Result<[u8; ETHER_ADDR_LEN as usize]> {
        let mac = mac_address_by_name(&self.name()?)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?
            .ok_or(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid mac address",
            ))?;
        Ok(mac.bytes())
    }
}

impl From<Layer> for c_short {
    fn from(layer: Layer) -> Self {
        match layer {
            Layer::L2 => 2,
            Layer::L3 => 3,
        }
    }
}
