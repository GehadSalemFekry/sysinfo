//
// Sysinfo
//
// Copyright (c) 2015 Guillaume Gomez
//

use sys::component::Component;
use sys::disk::Disk;
use sys::ffi;
use sys::network::{self, NetworkData};
use sys::process::*;
use sys::processor::*;

use {DiskExt, Pid, ProcessExt, ProcessorExt, RefreshKind, SystemExt};

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::mem;
use std::sync::Arc;
use sys::processor;

use libc::{self, c_int, c_void, size_t, sysconf, _SC_PAGESIZE};

use rayon::prelude::*;

/// Structs containing system's information.
pub struct System {
    process_list: HashMap<Pid, Process>,
    mem_total: u64,
    mem_free: u64,
    swap_total: u64,
    swap_free: u64,
    processors: Vec<Processor>,
    page_size_kb: u64,
    temperatures: Vec<Component>,
    connection: Option<ffi::io_connect_t>,
    disks: Vec<Disk>,
    network: NetworkData,
    uptime: u64,
    port: ffi::mach_port_t,
}

impl Drop for System {
    fn drop(&mut self) {
        if let Some(conn) = self.connection {
            unsafe {
                ffi::IOServiceClose(conn);
            }
        }
    }
}

pub(crate) struct Wrap<'a>(pub UnsafeCell<&'a mut HashMap<Pid, Process>>);

unsafe impl<'a> Send for Wrap<'a> {}
unsafe impl<'a> Sync for Wrap<'a> {}

impl System {
    fn clear_procs(&mut self) {
        let mut to_delete = Vec::new();

        for (pid, mut proc_) in &mut self.process_list {
            if !has_been_updated(&mut proc_) {
                to_delete.push(*pid);
            }
        }
        for pid in to_delete {
            self.process_list.remove(&pid);
        }
    }
}

impl SystemExt for System {
    fn new_with_specifics(refreshes: RefreshKind) -> System {
        let mut s = System {
            process_list: HashMap::with_capacity(200),
            mem_total: 0,
            mem_free: 0,
            swap_total: 0,
            swap_free: 0,
            processors: Vec::with_capacity(4),
            page_size_kb: unsafe { sysconf(_SC_PAGESIZE) as u64 >> 10 }, // divide by 1024
            temperatures: Vec::with_capacity(2),
            connection: get_io_service_connection(),
            disks: Vec::with_capacity(1),
            network: network::new(),
            uptime: get_uptime(),
            port: unsafe { ffi::mach_host_self() },
        };
        s.refresh_specifics(refreshes);
        s
    }

    fn refresh_memory(&mut self) {
        let mut mib = [0, 0];

        self.uptime = get_uptime();
        unsafe {
            // get system values
            // get swap info
            let mut xs: ffi::xsw_usage = mem::zeroed::<ffi::xsw_usage>();
            if get_sys_value(
                ffi::CTL_VM,
                ffi::VM_SWAPUSAGE,
                mem::size_of::<ffi::xsw_usage>(),
                &mut xs as *mut ffi::xsw_usage as *mut c_void,
                &mut mib,
            ) {
                self.swap_total = xs.xsu_total >> 10; // divide by 1024
                self.swap_free = xs.xsu_avail >> 10; // divide by 1024
            }
            // get ram info
            if self.mem_total < 1 {
                get_sys_value(
                    ffi::CTL_HW,
                    ffi::HW_MEMSIZE,
                    mem::size_of::<u64>(),
                    &mut self.mem_total as *mut u64 as *mut c_void,
                    &mut mib,
                );
                self.mem_total >>= 10; // divide by 1024
            }
            let count: u32 = ffi::HOST_VM_INFO64_COUNT;
            let mut stat = mem::zeroed::<ffi::vm_statistics64>();
            if ffi::host_statistics64(
                self.port,
                ffi::HOST_VM_INFO64,
                &mut stat as *mut ffi::vm_statistics64 as *mut c_void,
                &count,
            ) == ffi::KERN_SUCCESS
            {
                // From the apple documentation:
                //
                // /*
                //  * NB: speculative pages are already accounted for in "free_count",
                //  * so "speculative_count" is the number of "free" pages that are
                //  * used to hold data that was read speculatively from disk but
                //  * haven't actually been used by anyone so far.
                //  */
                // self.mem_free = u64::from(stat.free_count) * self.page_size_kb;
                self.mem_free = self.mem_total
                    - (u64::from(stat.active_count)
                        + u64::from(stat.inactive_count)
                        + u64::from(stat.wire_count)
                        + u64::from(stat.speculative_count)
                        - u64::from(stat.purgeable_count))
                        * self.page_size_kb;
            }
        }
    }

    fn refresh_temperatures(&mut self) {
        if let Some(con) = self.connection {
            if self.temperatures.is_empty() {
                // getting CPU critical temperature
                let critical_temp = crate::mac::component::get_temperature(
                    con,
                    &['T' as i8, 'C' as i8, '0' as i8, 'D' as i8, 0],
                );

                for (id, v) in crate::mac::component::COMPONENTS_TEMPERATURE_IDS.iter() {
                    if let Some(c) = Component::new(id.to_string(), None, critical_temp, v, con) {
                        self.temperatures.push(c);
                    }
                }
            } else {
                for comp in &mut self.temperatures {
                    comp.update(con);
                }
            }
        }
    }

    fn refresh_cpu(&mut self) {
        self.uptime = get_uptime();

        let mut mib = [0, 0];
        unsafe {
            // get processor values
            let mut num_cpu_u = 0u32;
            let mut cpu_info: *mut i32 = ::std::ptr::null_mut();
            let mut num_cpu_info = 0u32;

            if self.processors.is_empty() {
                let mut num_cpu = 0;

                if !get_sys_value(
                    ffi::CTL_HW,
                    ffi::HW_NCPU,
                    mem::size_of::<u32>(),
                    &mut num_cpu as *mut usize as *mut c_void,
                    &mut mib,
                ) {
                    num_cpu = 1;
                }

                self.processors.push(processor::create_proc(
                    "0".to_owned(),
                    Arc::new(ProcessorData::new(::std::ptr::null_mut(), 0)),
                ));
                if ffi::host_processor_info(
                    self.port,
                    ffi::PROCESSOR_CPU_LOAD_INFO,
                    &mut num_cpu_u as *mut u32,
                    &mut cpu_info as *mut *mut i32,
                    &mut num_cpu_info as *mut u32,
                ) == ffi::KERN_SUCCESS
                {
                    let proc_data = Arc::new(ProcessorData::new(cpu_info, num_cpu_info));
                    for i in 0..num_cpu {
                        let mut p =
                            processor::create_proc(format!("{}", i + 1), Arc::clone(&proc_data));
                        let in_use = *cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_USER as isize,
                        ) + *cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_SYSTEM as isize,
                        ) + *cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_NICE as isize,
                        );
                        let total = in_use
                            + *cpu_info.offset(
                                (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_IDLE as isize,
                            );
                        processor::set_cpu_proc(&mut p, in_use as f32 / total as f32);
                        self.processors.push(p);
                    }
                }
            } else if ffi::host_processor_info(
                self.port,
                ffi::PROCESSOR_CPU_LOAD_INFO,
                &mut num_cpu_u as *mut u32,
                &mut cpu_info as *mut *mut i32,
                &mut num_cpu_info as *mut u32,
            ) == ffi::KERN_SUCCESS
            {
                let mut pourcent = 0f32;
                let proc_data = Arc::new(ProcessorData::new(cpu_info, num_cpu_info));
                for (i, proc_) in self.processors.iter_mut().skip(1).enumerate() {
                    let old_proc_data = &*processor::get_processor_data(proc_);
                    let in_use =
                        (*cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_USER as isize,
                        ) - *old_proc_data.cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_USER as isize,
                        )) + (*cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_SYSTEM as isize,
                        ) - *old_proc_data.cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_SYSTEM as isize,
                        )) + (*cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_NICE as isize,
                        ) - *old_proc_data.cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_NICE as isize,
                        ));
                    let total = in_use
                        + (*cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_IDLE as isize,
                        ) - *old_proc_data.cpu_info.offset(
                            (ffi::CPU_STATE_MAX * i) as isize + ffi::CPU_STATE_IDLE as isize,
                        ));
                    processor::update_proc(
                        proc_,
                        in_use as f32 / total as f32,
                        Arc::clone(&proc_data),
                    );
                    pourcent += proc_.get_cpu_usage();
                }
                if self.processors.len() > 1 {
                    let len = self.processors.len() - 1;
                    if let Some(p) = self.processors.get_mut(0) {
                        processor::set_cpu_usage(p, pourcent / len as f32);
                    }
                }
            }
        }
    }

    fn refresh_network(&mut self) {
        network::update_network(&mut self.network);
    }

    fn refresh_processes(&mut self) {
        let count = unsafe { ffi::proc_listallpids(::std::ptr::null_mut(), 0) };
        if count < 1 {
            return;
        }
        if let Some(pids) = get_proc_list() {
            let arg_max = get_arg_max();
            let entries: Vec<Process> = {
                let wrap = &Wrap(UnsafeCell::new(&mut self.process_list));
                pids.par_iter()
                    .flat_map(|pid| match update_process(wrap, *pid, arg_max as size_t) {
                        Ok(x) => x,
                        Err(_) => None,
                    })
                    .collect()
            };
            entries.into_iter().for_each(|entry| {
                self.process_list.insert(entry.pid(), entry);
            });
            self.clear_procs();
        }
    }

    fn refresh_process(&mut self, pid: Pid) -> bool {
        let arg_max = get_arg_max();
        match {
            let wrap = Wrap(UnsafeCell::new(&mut self.process_list));
            update_process(&wrap, pid, arg_max as size_t)
        } {
            Ok(Some(p)) => {
                self.process_list.insert(p.pid(), p);
                true
            }
            Ok(_) => true,
            Err(_) => false,
        }
    }

    fn refresh_disks(&mut self) {
        for disk in &mut self.disks {
            disk.update();
        }
    }

    fn refresh_disk_list(&mut self) {
        self.disks = crate::mac::disk::get_disks();
    }

    // COMMON PART
    //
    // Need to be moved into a "common" file to avoid duplication.

    fn get_process_list(&self) -> &HashMap<Pid, Process> {
        &self.process_list
    }

    fn get_process(&self, pid: Pid) -> Option<&Process> {
        self.process_list.get(&pid)
    }

    fn get_processor_list(&self) -> &[Processor] {
        &self.processors[..]
    }

    fn get_network(&self) -> &NetworkData {
        &self.network
    }

    fn get_total_memory(&self) -> u64 {
        self.mem_total
    }

    fn get_free_memory(&self) -> u64 {
        self.mem_free
    }

    fn get_used_memory(&self) -> u64 {
        self.mem_total - self.mem_free
    }

    fn get_total_swap(&self) -> u64 {
        self.swap_total
    }

    fn get_free_swap(&self) -> u64 {
        self.swap_free
    }

    // need to be checked
    fn get_used_swap(&self) -> u64 {
        self.swap_total - self.swap_free
    }

    fn get_components_list(&self) -> &[Component] {
        &self.temperatures[..]
    }

    fn get_disks(&self) -> &[Disk] {
        &self.disks[..]
    }

    fn get_uptime(&self) -> u64 {
        self.uptime
    }
}

impl Default for System {
    fn default() -> System {
        System::new()
    }
}

// code from https://github.com/Chris911/iStats
fn get_io_service_connection() -> Option<ffi::io_connect_t> {
    let mut master_port: ffi::mach_port_t = 0;
    let mut iterator: ffi::io_iterator_t = 0;

    unsafe {
        ffi::IOMasterPort(ffi::MACH_PORT_NULL, &mut master_port);

        let matching_dictionary = ffi::IOServiceMatching(b"AppleSMC\0".as_ptr() as *const i8);
        let result =
            ffi::IOServiceGetMatchingServices(master_port, matching_dictionary, &mut iterator);
        if result != ffi::KIO_RETURN_SUCCESS {
            //println!("Error: IOServiceGetMatchingServices() = {}", result);
            return None;
        }

        let device = ffi::IOIteratorNext(iterator);
        ffi::IOObjectRelease(iterator);
        if device == 0 {
            //println!("Error: no SMC found");
            return None;
        }

        let mut conn = 0;
        let result = ffi::IOServiceOpen(device, ffi::mach_task_self(), 0, &mut conn);
        ffi::IOObjectRelease(device);
        if result != ffi::KIO_RETURN_SUCCESS {
            //println!("Error: IOServiceOpen() = {}", result);
            return None;
        }

        Some(conn)
    }
}

fn get_uptime() -> u64 {
    let mut boottime: libc::timeval = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<libc::timeval>();
    let mut mib: [c_int; 2] = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    unsafe {
        if libc::sysctl(
            mib.as_mut_ptr(),
            2,
            &mut boottime as *mut libc::timeval as *mut _,
            &mut len,
            ::std::ptr::null_mut(),
            0,
        ) < 0
        {
            return 0;
        }
    }
    let bsec = boottime.tv_sec;
    let csec = unsafe { libc::time(::std::ptr::null_mut()) };

    unsafe { libc::difftime(csec, bsec) as u64 }
}

fn get_arg_max() -> usize {
    let mut mib: [c_int; 3] = [libc::CTL_KERN, libc::KERN_ARGMAX, 0];
    let mut arg_max = 0i32;
    let mut size = mem::size_of::<c_int>();
    unsafe {
        if libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut arg_max) as *mut i32 as *mut c_void,
            &mut size,
            ::std::ptr::null_mut(),
            0,
        ) == -1
        {
            4096 // We default to this value
        } else {
            arg_max as usize
        }
    }
}

unsafe fn get_sys_value(
    high: u32,
    low: u32,
    mut len: usize,
    value: *mut c_void,
    mib: &mut [i32; 2],
) -> bool {
    mib[0] = high as i32;
    mib[1] = low as i32;
    libc::sysctl(
        mib.as_mut_ptr(),
        2,
        value,
        &mut len as *mut usize,
        ::std::ptr::null_mut(),
        0,
    ) == 0
}
