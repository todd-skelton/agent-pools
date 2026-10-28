//! `pools-rs.exe`: the program with no console window, for the scheduled tasks. Task Scheduler
//! starts it directly (a console program would open a window, which is why Agent Pools went
//! through wscript.exe and a .vbs). Its output goes where its parent sends it: nowhere from a
//! task, `logs/supervisor.out.log` for the service the guardian starts.
#![windows_subsystem = "windows"]

fn main() -> std::io::Result<()> {
    pools_shadow::run()
}
