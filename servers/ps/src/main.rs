//! ps — list processes via SYS_PROC_LIST. The kernel is the only thing that knows
//! the process table, so this is a thin formatter over its snapshot. The syscall is
//! pledge-gated (PLEDGE_PROC): a confined program can't enumerate other processes.
#![no_std]
#![no_main]

use oxbow_abi::{ProcInfo, PROC_NAME_LEN, PROC_STATE_ALIVE};
use oxbow_rt as rt;

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let mut buf = [ProcInfo { pid: 0, state: 0, name: [0u8; PROC_NAME_LEN] }; 32];
    let n = rt::sys_proc_list(&mut buf);
    rt::println!("  PID STAT CMD");
    for p in &buf[..n] {
        let st = if p.state == PROC_STATE_ALIVE { "R" } else { "Z" };
        let end = p.name.iter().position(|&b| b == 0).unwrap_or(PROC_NAME_LEN);
        rt::print!("{:>5} {:<4} ", p.pid, st);
        rt::stdout_write(&p.name[..end]);
        rt::println!("");
    }
    rt::sys_exit(0)
}
