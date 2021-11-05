use crate::network::types::NetAddress;
use crate::network::{core_utils, types};
use ipnet;
use log::debug;
use log::warn;
use nix::sched;
use rand::Rng;
use std::collections::HashMap;
use std::fs::File;
use std::io::Error;
use std::net::IpAddr;
use std::os::unix::prelude::*;
use std::thread;

pub struct Core {
    pub networkns: String,
}

impl Core {
    pub fn bridge_per_podman_network(
        per_network_opts: &types::PerNetworkOptions,
        network: &types::Network,
        netns: &str,
    ) -> Result<types::StatusBlock, std::io::Error> {
        //  StatusBlock response
        let mut response = types::StatusBlock {
            dns_search_domains: Some(Vec::new()),
            dns_server_ips: Some(Vec::new()),
            interfaces: Some(HashMap::new()),
        };
        // get bridge name
        let bridge_name: String = network.network_interface.as_ref().unwrap().to_owned();
        // create a vector for all subnet masks
        let mut subnet_mask_vector = Vec::new();
        // static ip vector
        let mut address_vector = Vec::new();
        // gateway ip vector
        let mut gw_ipaddr_vector = Vec::new();
        // network addresses for response
        let mut response_net_addresses: Vec<NetAddress> = Vec::new();
        // interfaces map, but we only ever expect one, for response
        let mut interfaces: HashMap<String, types::NetInterface> = HashMap::new();

        let container_veth_name: String = per_network_opts.interface_name.to_owned();
        let static_ips: &Vec<IpAddr> = per_network_opts.static_ips.as_ref().unwrap();

        //we have the bridge name but we must iterate for all the available gateways
        for (idx, subnet) in network.subnets.iter().flatten().enumerate() {
            gw_ipaddr_vector.push(subnet.gateway.as_ref().unwrap().to_owned());
            let subnet_mask = subnet.subnet.netmask().to_string();
            let subnet_mask_cidr = subnet.subnet.prefix_len();

            // Build up response information
            let container_address: ipnet::IpNet =
                match format!("{}/{}", static_ips[idx].to_string(), subnet_mask_cidr).parse() {
                    Ok(i) => i,
                    Err(e) => {
                        return Err(Error::new(std::io::ErrorKind::Other, e));
                    }
                };
            // Add the IP to the address_vector
            address_vector.push(static_ips[idx].to_string());
            response_net_addresses.push(types::NetAddress {
                gateway: subnet.gateway.to_owned(),
                subnet: container_address,
            });
            subnet_mask_vector.push(subnet_mask);
        }
        debug!("Container veth name: {:?}", container_veth_name);
        debug!("Brige name: {:?}", bridge_name);
        debug!("Subnet masks vector: {:?}", subnet_mask_vector);
        debug!("IP address for veth vector: {:?}", address_vector);
        debug!("Gateway ip address vector: {:?}", gw_ipaddr_vector);

        let container_veth_mac = match Core::add_bridge_and_veth(
            &bridge_name,
            address_vector,
            subnet_mask_vector,
            gw_ipaddr_vector,
            &container_veth_name,
            netns,
        ) {
            Ok(addr) => addr,
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed configure bridge and veth interface: {:?}", err),
                ))
            }
        };
        debug!("Container veth mac: {:?}", container_veth_mac);
        let interface = types::NetInterface {
            mac_address: container_veth_mac,
            networks: Option::from(response_net_addresses),
        };
        // Add interface to interfaces (part of StatusBlock)
        interfaces.insert(container_veth_name, interface);
        let _ = response.interfaces.insert(interfaces);
        Ok(response)
    }

    pub fn add_bridge_and_veth(
        br_name: &str,
        netns_ipaddr: Vec<String>,
        netns_ipaddr_mask: Vec<String>,
        gw_ipaddr: Vec<String>,
        container_veth_name: &str,
        netns: &str,
    ) -> Result<String, std::io::Error> {
        //copy subnet masks and gateway ips since we are going to use it later
        let mut netns_ipaddr_mask_clone = Vec::new();
        let mut gw_ipaddr_clone = Vec::new();
        for mask in &netns_ipaddr_mask {
            let mask_add: String = mask.to_owned().to_string();
            netns_ipaddr_mask_clone.push(mask_add)
        }
        for gw_ip in &gw_ipaddr {
            let gw_ip_add: String = gw_ip.to_owned().to_string();
            gw_ipaddr_clone.push(gw_ip_add)
        }
        //call configure bridge
        let _ = match core_utils::CoreUtils::configure_bridge_async(
            br_name,
            gw_ipaddr,
            netns_ipaddr_mask,
        ) {
            Ok(_) => (),
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Error while configuring network interface {}:", err),
                ))
            }
        };

        let host_veth_name = format!("veth{:x}", rand::thread_rng().gen::<u32>());

        let _ = match core_utils::CoreUtils::configure_veth_async(
            &host_veth_name,
            container_veth_name,
            br_name,
            netns,
        ) {
            Ok(_) => (),
            Err(err) => {
                // it seems something went wrong
                // we must not leave dangling interfaces
                // otherwise cleanup would become mess
                // try removing leaking interfaces from host
                if let Err(er) = core_utils::CoreUtils::remove_interface(&host_veth_name) {
                    warn!("Failed while cleaning up interfaces: {}", er);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Error while configuring network interface {}:", err),
                ));
            }
        };

        //bridge and veth configured successfully
        //do we want mac ?
        //TODO: we can verify MAC here

        match File::open(&netns) {
            Ok(netns_file) => {
                let netns_fd = netns_file.as_raw_fd();
                //clone values before spwaning thread in new namespace
                let container_veth_name_clone: String = container_veth_name.to_owned();
                // So complicated cloning for threads ?
                // TODO: simplify this later
                let mut netns_ipaddr_clone = Vec::new();
                for ip in &netns_ipaddr {
                    let ip_add: String = ip.to_owned().to_string();
                    netns_ipaddr_clone.push(ip_add)
                }
                let handle = thread::spawn(move || -> Result<String, Error> {
                    if let Err(err) = sched::setns(netns_fd, sched::CloneFlags::CLONE_NEWNET) {
                        panic!("failed to setns to fd={}: {}", netns_fd, err);
                    }

                    if let Err(err) = core_utils::CoreUtils::configure_netns_interface_async(
                        &container_veth_name_clone,
                        netns_ipaddr_clone,
                        netns_ipaddr_mask_clone,
                        gw_ipaddr_clone,
                    ) {
                        return Err(err);
                    }
                    debug!(
                        "Configured static up address for {}",
                        container_veth_name_clone
                    );

                    if let Err(er) = core_utils::CoreUtils::turn_up_interface("lo") {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("failed while turning up `lo` in container namespace {}", er),
                        ));
                    }

                    //return MAC address to status block could use this
                    match core_utils::CoreUtils::get_interface_address(&container_veth_name_clone) {
                        Ok(addr) => Ok(addr),
                        Err(err) => Err(err),
                    }
                });
                match handle.join() {
                    Ok(interface_address) => interface_address,
                    Err(err) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("failed to join: {:?}", err),
                        ))
                    }
                }
            }
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed to open the netns file: {}", err),
                ))
            }
        }
    }

    pub fn remove_interface_per_podman_network(
        network_opts: &types::NetworkOptions,
        netns: &str,
    ) -> Result<(), std::io::Error> {
        for (net_name, network) in &network_opts.network_info {
            // get network name
            let network_name: String = net_name.to_owned();
            // get PerNetworkOptions for this network
            let network_per_opts = network_opts.networks.get(&network_name);
            let container_veth_name: String = network_per_opts.unwrap().interface_name.to_owned();
            let _subnets = network.subnets.as_ref().unwrap();

            debug!(
                "Container veth name being removed: {:?}",
                container_veth_name
            );

            if let Err(err) = Core::remove_container_veth(&container_veth_name, netns) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("unable to remove container veth: {:?}", err),
                ));
            }

            debug!("Container veth removed: {:?}", container_veth_name);
        }

        Ok(())
    }

    fn remove_container_veth(ifname: &str, netns: &str) -> Result<(), std::io::Error> {
        match File::open(netns) {
            Ok(file) => {
                let netns_fd = file.as_raw_fd();
                let container_veth: String = ifname.to_owned();
                let handle = thread::spawn(move || -> Result<(), Error> {
                    if let Err(err) = sched::setns(netns_fd, sched::CloneFlags::CLONE_NEWNET) {
                        panic!(
                            "{}",
                            format!(
                                "failed to setns on container network namespace fd={}: {}",
                                netns_fd, err
                            )
                        )
                    }

                    if let Err(err) = core_utils::CoreUtils::remove_interface(&container_veth) {
                        return Err(err);
                    }

                    Ok(())
                });
                if let Err(err) = handle.join() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("unable to join thread: {:?}", err),
                    ));
                }
            }
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed to open network namespace: {}", err),
                ))
            }
        };

        Ok(())
    }
}
