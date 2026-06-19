//! oxbow std::process — spawn a child program (std reads its ELF via std::fs),
//! inheriting the parent's stdio/cwd/net caps, and wait on its exit notification.
//! Piped stdio (output capture) + kill + try_wait are not wired yet.
use super::env::{CommandEnv, CommandEnvs, CommandResolvedEnvs};
pub use crate::ffi::OsString as EnvKey;
use crate::ffi::{OsStr, OsString};
use crate::num::NonZero;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::sys::fs::File;
use crate::sys::unsupported;
use crate::{fmt, io};

unsafe extern "C" {
    fn __oxbow_spawn(
        elf: *const u8,
        elf_len: usize,
        argv: *const u8,
        argv_len: usize,
        pid_out: *mut u32,
    ) -> i64;
    fn __oxbow_wait(notif: i64) -> i32;
}

pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: CommandEnv,
    cwd: Option<OsString>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

#[derive(Debug)]
pub enum Stdio {
    Inherit,
    Null,
    MakePipe,
    ParentStdout,
    ParentStderr,
    #[allow(dead_code)]
    InheritFile(File),
}

impl Command {
    pub fn new(program: &OsStr) -> Command {
        Command {
            program: program.to_owned(),
            args: vec![program.to_owned()],
            env: Default::default(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }
    pub fn arg(&mut self, arg: &OsStr) {
        self.args.push(arg.to_owned());
    }
    pub fn env_mut(&mut self) -> &mut CommandEnv {
        &mut self.env
    }
    pub fn cwd(&mut self, dir: &OsStr) {
        self.cwd = Some(dir.to_owned());
    }
    pub fn stdin(&mut self, stdin: Stdio) {
        self.stdin = Some(stdin);
    }
    pub fn stdout(&mut self, stdout: Stdio) {
        self.stdout = Some(stdout);
    }
    pub fn stderr(&mut self, stderr: Stdio) {
        self.stderr = Some(stderr);
    }
    pub fn get_program(&self) -> &OsStr {
        &self.program
    }
    pub fn get_args(&self) -> CommandArgs<'_> {
        let mut iter = self.args.iter();
        iter.next();
        CommandArgs { iter }
    }
    pub fn get_envs(&self) -> CommandEnvs<'_> {
        self.env.iter()
    }
    pub fn get_env_clear(&self) -> bool {
        self.env.does_clear()
    }
    pub fn get_resolved_envs(&self) -> CommandResolvedEnvs {
        CommandResolvedEnvs::new(self.env.capture())
    }
    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_ref().map(|cs| Path::new(cs))
    }

    pub fn spawn(
        &mut self,
        _default: Stdio,
        _needs_stdin: bool,
    ) -> io::Result<(Process, StdioPipes)> {
        // Resolve a bare program name to /bin/<name> (a path passes through).
        // Resolve relative to the program's cwd cap (the user namespace). /bin
        // (the shell's system tools) is NOT reachable from a user process.
        let path = self.program.to_string_lossy().into_owned();
        let elf = crate::fs::read(&path)
            .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "command not found"))?;
        // oxbow argv = the args AFTER the program name, space-joined.
        let mut argv = String::new();
        for (i, a) in self.args.iter().enumerate().skip(1) {
            if i > 1 {
                argv.push(' ');
            }
            argv.push_str(&a.to_string_lossy());
        }
        let mut pid: u32 = 0;
        let notif = unsafe {
            __oxbow_spawn(elf.as_ptr(), elf.len(), argv.as_ptr(), argv.len(), &mut pid)
        };
        if notif < 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "oxbow: spawn rejected"));
        }
        Ok((
            Process { notif, pid },
            StdioPipes { stdin: None, stdout: None, stderr: None },
        ))
    }
}

pub fn output(cmd: &mut Command) -> io::Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    // Inherit stdio (no pipe capture yet): the child's output goes to the terminal.
    let (mut proc, _pipes) = cmd.spawn(Stdio::Inherit, false)?;
    let status = proc.wait()?;
    Ok((status, Vec::new(), Vec::new()))
}

impl From<ChildPipe> for Stdio {
    fn from(pipe: ChildPipe) -> Stdio {
        pipe.diverge()
    }
}
impl From<io::Stdout> for Stdio {
    fn from(_: io::Stdout) -> Stdio {
        Stdio::ParentStdout
    }
}
impl From<io::Stderr> for Stdio {
    fn from(_: io::Stderr) -> Stdio {
        Stdio::ParentStderr
    }
}
impl From<File> for Stdio {
    fn from(file: File) -> Stdio {
        Stdio::InheritFile(file)
    }
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.args[0])?;
        for arg in &self.args[1..] {
            write!(f, " {arg:?}")?;
        }
        Ok(())
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitStatus(i32);

impl Default for ExitStatus {
    fn default() -> Self {
        ExitStatus(0)
    }
}
impl ExitStatus {
    pub fn exit_ok(&self) -> Result<(), ExitStatusError> {
        if self.0 == 0 { Ok(()) } else { Err(ExitStatusError(self.0)) }
    }
    pub fn code(&self) -> Option<i32> {
        Some(self.0)
    }
}
impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit code: {}", self.0)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitStatusError(i32);
impl Into<ExitStatus> for ExitStatusError {
    fn into(self) -> ExitStatus {
        ExitStatus(self.0)
    }
}
impl ExitStatusError {
    pub fn code(self) -> Option<NonZero<i32>> {
        NonZero::new(self.0)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitCode(u8);
impl ExitCode {
    pub const SUCCESS: ExitCode = ExitCode(0);
    pub const FAILURE: ExitCode = ExitCode(1);
    pub fn as_i32(&self) -> i32 {
        self.0 as i32
    }
}
impl From<u8> for ExitCode {
    fn from(code: u8) -> Self {
        Self(code)
    }
}

pub struct Process {
    notif: i64,
    pid: u32,
}
impl Process {
    pub fn id(&self) -> u32 {
        self.pid
    }
    pub fn kill(&mut self) -> io::Result<()> {
        unsupported()
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        Ok(ExitStatus(unsafe { __oxbow_wait(self.notif) }))
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(None) // no non-blocking wait yet
    }
}

pub struct CommandArgs<'a> {
    iter: crate::slice::Iter<'a, OsString>,
}
impl<'a> Iterator for CommandArgs<'a> {
    type Item = &'a OsStr;
    fn next(&mut self) -> Option<&'a OsStr> {
        self.iter.next().map(|os| &**os)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}
impl<'a> ExactSizeIterator for CommandArgs<'a> {
    fn len(&self) -> usize {
        self.iter.len()
    }
    fn is_empty(&self) -> bool {
        self.iter.is_empty()
    }
}
impl<'a> fmt::Debug for CommandArgs<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone()).finish()
    }
}

pub type ChildPipe = crate::sys::pipe::Pipe;

pub fn read_output(
    out: ChildPipe,
    _stdout: &mut Vec<u8>,
    _err: ChildPipe,
    _stderr: &mut Vec<u8>,
) -> io::Result<()> {
    match out.diverge() {}
}

pub fn getpid() -> u32 {
    0
}

pub fn getppid() -> u32 {
    0
}
