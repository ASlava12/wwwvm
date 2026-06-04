//! Two real Alpine guests on one virtual LAN, talking to each other through the
//! `crates/net` L2 switch — the native proof that the parallel-networked-VMs
//! path works end to end (NIC TX/RX seam + learning switch + per-VM MAC/IP).
//!
//! Each VM gets a distinct MAC (`set_nic_mac`) and a distinct IP from the kernel
//! cmdline (`wwwvm.ip=10.0.0.N/24`, read by the WWWVM_NET_LAN `/init`). We step
//! both, drain each one's transmitted Ethernet frames, route them through the
//! switch, and inject into the destination VM — then have VM 1 `ping` VM 2 and
//! watch for replies, which can only arrive if ARP + ICMP crossed the switch.
//!
//! Prereqs (the same assets the browser lab uses):
//!   scripts/fetch-alpine-assets.sh --with-net    # eth0 modules
//!   WWWVM_NET_LAN=1 WWWVM_DUMP_INITRAMFS=/tmp/wwwvm-lan.cpio \
//!     WWWVM_ALPINE_MINIROOT=/tmp/alpine/root cargo run --example alpine_console
//!   WWWVM_KERNEL=/tmp/wwwvm-alpine/vmlinuz-lts WWWVM_LAN_CPIO=/tmp/wwwvm-lan.cpio \
//!     cargo run --release --example two_vm_lan -p wwwvm-vm

use std::env;
use std::fs;

use wwwvm_net::switch::Switch;
use wwwvm_vm::Vm;

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn main() {
    let kernel_path =
        env::var("WWWVM_KERNEL").unwrap_or_else(|_| "/tmp/wwwvm-alpine/vmlinuz-lts".into());
    let cpio_path = env::var("WWWVM_LAN_CPIO").unwrap_or_else(|_| "/tmp/wwwvm-lan.cpio".into());
    let kernel = fs::read(&kernel_path).unwrap_or_else(|e| panic!("read {kernel_path}: {e}"));
    let cpio = fs::read(&cpio_path).unwrap_or_else(|e| panic!("read {cpio_path}: {e}"));

    const N: usize = 2;
    let mut vms: Vec<Vm> = Vec::new();
    for i in 0..N {
        let mut vm = Vm::with_ram_size(256 * 1024 * 1024);
        // Distinct MAC + IP per VM so they're real, separate LAN hosts.
        vm.set_nic_mac([0x52, 0x54, 0x00, 0x00, 0x00, (i + 1) as u8]);
        let bz = vm.load_bzimage(&kernel).expect("load_bzimage");
        vm.set_kernel_cmdline(&format!(
            "earlyprintk=ttyS0,115200 console=ttyS0 panic=10 lpj=1000000 loglevel=4 \
             wwwvm.ip=10.0.0.{}/24",
            i + 1
        ));
        vm.set_ramdisk(&cpio).expect("set_ramdisk");
        vm.start_protected_mode_at(bz.code32_start);
        vms.push(vm);
    }

    // Send the ping ONLY after both shells are ready — input sent during boot
    // gets partially eaten by the kernel/early console (the line arrives
    // corrupted), so we wait for the readiness banner on BOTH VMs first.
    let mut sw = Switch::new();
    let mut out: [Vec<u8>; N] = Default::default();
    let mut ready = [false; N];
    let mut sent = false;
    let mut done = false;
    // ~2M steps/VM/round; cap well above a boot (~2.5B) + the ping phase.
    for round in 0..6000u32 {
        for vm in vms.iter_mut() {
            vm.run_steps_idle_aware(2_000_000);
        }
        // Drain every VM's TX, route through the switch, inject into the
        // destination VM(s). Two passes keep the &mut borrows non-aliasing.
        let mut deliveries: Vec<(usize, Vec<u8>)> = Vec::new();
        for (i, vm) in vms.iter_mut().enumerate() {
            for f in vm.drain_tx_frames() {
                for eg in sw.egress(i, &f, N) {
                    deliveries.push((eg, f.clone()));
                }
            }
        }
        for (eg, f) in deliveries {
            vms[eg].inject_rx_frame(&f);
        }
        for (i, vm) in vms.iter_mut().enumerate() {
            out[i].extend(vm.drain_output());
            if contains(&out[i], b"shell ready") {
                ready[i] = true;
            }
        }
        if !sent && ready.iter().all(|&r| r) {
            eprintln!("[two_vm_lan] both shells ready at round {round} — pinging");
            // NB: the tty echoes the command line, so a marker like `echo X` in
            // the command would appear in output immediately — detect completion
            // on ping's OWN stats line ("packets transmitted") instead.
            vms[0].send_input(b"ip -o -4 addr show eth0; ip route; ping -c2 10.0.0.2\n");
            sent = true;
        }
        // ping prints "N packets transmitted, M packets received" when done.
        if sent && contains(&out[0], b"packets transmitted") {
            done = true;
            eprintln!("[two_vm_lan] ping finished after {round} rounds");
            break;
        }
    }
    let out0 = &out[0];

    let text = String::from_utf8_lossy(out0);
    // Print the tail (the addr/route/ping diagnostics).
    println!("---- VM1 console (tail) ----");
    let start = text.len().saturating_sub(1400);
    for line in text[start..].lines() {
        println!("VM1> {line}");
    }
    println!("----------------------------");

    let replies = text.matches("bytes from 10.0.0.2").count();
    let ok = replies > 0;
    let verdict = if ok {
        "PASS — VM1 pinged VM2 across the L2 switch"
    } else {
        "no replies"
    };
    println!("\n=== RESULT: {verdict} ({replies} ICMP replies, completed: {done}) ===");
    std::process::exit(if ok { 0 } else { 1 });
}
