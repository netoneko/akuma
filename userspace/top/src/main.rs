#![no_std]
#![no_main]

extern crate alloc;

use libakuma::*;

// Mode flags for terminal attributes (mirroring kernel's terminal/mod.rs)
pub mod mode_flags {
    /// Enable raw mode (disable canonical, echo, ISIG)
    pub const RAW_MODE_ENABLE: u64 = 0x01;
    /// Disable raw mode (restore canonical, echo, ISIG)
    pub const RAW_MODE_DISABLE: u64 = 0x02;
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut once = false;
    for arg in args().skip(1) {
        if arg == "--once" || arg == "-n" || arg == "1" {
            once = true;
        }
    }

    let mut initial_mode_flags: u64 = 0;
    if !once {
        // Save initial terminal attributes
        get_terminal_attributes(
            fd::STDIN,
            &mut initial_mode_flags as *mut u64 as u64,
        );
        
        // Enable raw mode
        set_terminal_attributes(fd::STDIN, 0, mode_flags::RAW_MODE_ENABLE);
        
        clear_screen();
        hide_cursor();
    }

    let mut last_stats: [ThreadCpuStat; 64] = [ThreadCpuStat::default(); 64];
    let mut last_time = uptime();

    loop {
        let mut current_stats: [ThreadCpuStat; 64] = [ThreadCpuStat::default(); 64];
        let count = get_cpu_stats(&mut current_stats);
        let current_time = uptime();
        let delta_time = current_time.saturating_sub(last_time);

        if !once {
            set_cursor_position(0, 0);
        }

        println("Akuma OS - CPU Stats (press 'q' to quit)");
        println("TID  PID  STATE       CPU%   TIME(ms)  NAME");
        println("--------------------------------------------------");

        for i in 0..count {
            let cur = &current_stats[i];
            let last = &last_stats[i];
            
            if cur.state == 0 { continue; } // FREE

            let delta_cpu_time = cur.total_time_us.saturating_sub(last.total_time_us);
            let cpu_usage = if delta_time > 0 {
                (delta_cpu_time as f64 * 100.0) / (delta_time as f64)
            } else {
                0.0
            };

            let state_str = match cur.state {
                1 => "READY  ",
                2 => "RUNNING",
                3 => "EXITED ",
                4 => "INIT   ",
                5 => "WAITING",
                _ => "UNKNOWN",
            };

            print_u32_fixed(cur.tid, 3);
            print("  ");
            print_u32_fixed(cur.pid, 3);
            print("  ");
            print(state_str);
            print("  ");
            print_f64_fixed(cpu_usage, 1);
            print("%  ");
            print_u64_fixed(cur.total_time_us / 1000, 8);
            print("  ");
            
            // Print name
            let mut name_len = 0;
            while name_len < 16 && cur.name[name_len] != 0 {
                name_len += 1;
            }
            if let Ok(name) = core::str::from_utf8(&cur.name[..name_len]) {
                println(name);
            } else {
                println("???");
            }
        }

        if once {
            break;
        }

        last_stats = current_stats;
        last_time = current_time;

        // Check for 'q' to quit
        let mut input = [0u8; 1];
        if poll_input_event(1000, &mut input) > 0 {
            if input[0] == b'q' {
                break;
            }
        }
    }

    if !once {
        show_cursor();
        // Restore initial terminal attributes
        set_terminal_attributes(fd::STDIN, 0, initial_mode_flags);
        println("\n");
    }
    exit(0);
}

fn print_u32_fixed(val: u32, width: usize) {
    print_u64_fixed(val as u64, width);
}

fn print_u64_fixed(val: u64, width: usize) {
    let mut buf = [b' '; 20];
    let mut v = val;
    let mut i = 19;
    if v == 0 {
        buf[i] = b'0';
        i -= 1;
    } else {
        while v > 0 && i > 0 {
            buf[i] = b'0' + (v % 10) as u8;
            v /= 10;
            i -= 1;
        }
    }
    
    let start = 20usize.saturating_sub(width);
    for j in start..20 {
        let c = if j > i { buf[j] } else { b' ' };
        let s = core::slice::from_ref(&c);
        print(unsafe { core::str::from_utf8_unchecked(s) });
    }
}

fn print_f64_fixed(val: f64, _precision: usize) {
    let whole = val as u64;
    print_u64_fixed(whole, 3);
    print(".");
    let frac = ((val - whole as f64) * 10.0) as u64;
    print_u64_fixed(frac % 10, 1);
}
