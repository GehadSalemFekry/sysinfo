//
// Sysinfo
//
// Copyright (c) 2019 Guillaume Gomez
//
use std::fs;
use std::fs::File;
use std::io::{Error, ErrorKind, Read};

use NetworkExt;

/// Contains network information.
#[derive(Debug)]
pub struct NetworkData {
    old_in: u64,
    old_out: u64,
    current_in: u64,
    current_out: u64,
}

impl NetworkExt for NetworkData {
    fn get_income(&self) -> u64 {
        self.current_in - self.old_in
    }

    fn get_outcome(&self) -> u64 {
        self.current_out - self.old_out
    }
}

pub fn new() -> NetworkData {
    NetworkData {
        old_in: 0,
        old_out: 0,
        current_in: 0,
        current_out: 0,
    }
}

fn read_things() -> Result<(u64, u64), Error> {
    fn read_interface_stat(iface: &str, typ: &str) -> Result<u64, Error> {
        let mut file = File::open(format!("/sys/class/net/{}/statistics/{}_bytes", iface, typ))?;
        let mut content = String::with_capacity(20);
        file.read_to_string(&mut content)?;
        content
            .trim()
            .parse()
            .map_err(|_| Error::new(ErrorKind::Other, "Failed to parse network stat"))
    }

    let mut rx: u64 = 0;
    let mut tx: u64 = 0;
    if let Ok(entries) = fs::read_dir("/sys/class/net"){
        for entry in entries{
            if let Ok(entry) = entry{
                if let Ok(iface) = entry.file_name().into_string(){
                    rx += read_interface_stat(iface.as_str(), "rx").unwrap_or(0);
                    tx += read_interface_stat(iface.as_str(), "tx").unwrap_or(0);
                }
               
            }
        }
    }
    
    Ok((rx, tx))
}

pub fn update_network(n: &mut NetworkData) {
    if let Ok((new_in, new_out)) = read_things() {
        n.old_in = n.current_in;
        n.old_out = n.current_out;
        n.current_in = new_in;
        n.current_out = new_out;
    }
    // TODO: maybe handle error here?
}
