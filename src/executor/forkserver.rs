//! Expose an `Executor` based on a `Forkserver` in order to execute AFL/AFL++ binaries
//!
//! Copy of the LibAFL forkserver, but with support for using sockets as the input mode and support saving request response pairs
//! The main additions are the [`SocketConnector`] and changes in the trait method [`ForkserverExecutor::run_target`].
//!
//! [`SocketConnector`] has two modes, it can either act as a server or a client. The target should behave as the opposite.
//! Moreover, a [`crate::replay::RequestResponseCollector`] can be given to the Forkserver when it is constructed.
//! This only works if the inputmode is through a socket. With this collector, all messages are saved. This is a slow-down
//! and requires some space on the disk, but it is useful for testing, evaluation, debugging and crash triaging.

use core::{
    fmt::{self, Debug, Formatter},
    marker::PhantomData,
    time::Duration,
};
use std::{
    borrow::ToOwned,
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr},
    string::ToString,
    thread::sleep,
    vec::Vec,
};
use std::{
    ffi::{OsStr, OsString},
    io::{self, prelude::*, ErrorKind},
    net::{TcpListener, TcpStream},
    os::{
        fd::{AsRawFd, BorrowedFd},
        unix::{io::RawFd, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    thread::JoinHandle,
};

use libafl_bolts::{
    fs::{get_unique_std_input_file, InputFile},
    os::{dup2, pipes::Pipe},
    shmem::{ShMem, ShMemProvider, UnixShMemProvider},
    tuples::Prepend,
    AsMutSlice, AsSlice, Truncate,
};
use nix::{
    libc::{self},
    sys::{
        select::{pselect, FdSet},
        signal::{kill, SigSet, Signal},
        time::TimeSpec,
        wait::waitpid,
    },
    unistd::Pid,
};

use crate::{
    libaflstar_bolts::create_timeout_error,
    replay::{RequestResponseCollector, RequestResponsePair},
};
use libafl::{
    executors::{Executor, ExitKind, HasObservers},
    inputs::{HasTargetBytes, Input, UsesInput},
    mutators::Tokens,
    observers::{MapObserver, Observer, ObserversTuple, UsesObservers},
    state::{HasExecutions, State, UsesState},
    Error,
};

const FORKSRV_FD: i32 = 198;
#[allow(clippy::cast_possible_wrap)]
const FS_OPT_ENABLED: i32 = 0x80000001_u32 as i32;
#[allow(clippy::cast_possible_wrap)]
const FS_OPT_MAPSIZE: i32 = 0x40000000_u32 as i32;
#[allow(clippy::cast_possible_wrap)]
const FS_OPT_SHDMEM_FUZZ: i32 = 0x01000000_u32 as i32;
#[allow(clippy::cast_possible_wrap)]
const FS_OPT_AUTODICT: i32 = 0x10000000_u32 as i32;

// #[allow(clippy::cast_possible_wrap)]
// const FS_OPT_MAX_MAPSIZE: i32 = ((0x00fffffe_u32 >> 1) + 1) as i32; // 8388608
const fn fs_opt_get_mapsize(x: i32) -> i32 {
    ((x & 0x00fffffe) >> 1) + 1
}
/* const fn fs_opt_set_mapsize(x: usize) -> usize {
    if x <= 1 {
      if x > FS_OPT_MAX_MAPSIZE { 0 } else { (x - 1) << 1 }
    } else { 0 }
} */

/// The length of header bytes which tells shmem size
const SHMEM_FUZZ_HDR_SIZE: usize = 4;
const MAX_INPUT_SIZE_DEFAULT: usize = 1024 * 1024;

/// The default signal to use to kill child processes
const KILL_SIGNAL_DEFAULT: Signal = Signal::SIGTERM;

/// Configure the target, `limit`, `setsid`, `pipe_stdin`, the code was borrowed from the [`Angora`](https://github.com/AngoraFuzzer/Angora) fuzzer
pub trait ConfigTarget {
    /// Sets the sid
    fn setsid(&mut self) -> &mut Self;
    /// Sets a mem limit
    fn setlimit(&mut self, memlimit: u64) -> &mut Self;
    /// Sets the stdin
    fn setstdin(&mut self, fd: RawFd, use_stdin: bool) -> &mut Self;
    /// Sets the AFL forkserver pipes
    fn setpipe(
        &mut self,
        st_read: RawFd,
        st_write: RawFd,
        ctl_read: RawFd,
        ctl_write: RawFd,
    ) -> &mut Self;
}

impl ConfigTarget for Command {
    fn setsid(&mut self) -> &mut Self {
        let func = move || {
            unsafe {
                libc::setsid();
            };
            Ok(())
        };
        unsafe { self.pre_exec(func) }
    }

    fn setpipe(
        &mut self,
        st_read: RawFd,
        st_write: RawFd,
        ctl_read: RawFd,
        ctl_write: RawFd,
    ) -> &mut Self {
        let func = move || {
            match dup2(ctl_read, FORKSRV_FD) {
                Ok(()) => (),
                Err(_) => {
                    return Err(io::Error::last_os_error());
                }
            }

            match dup2(st_write, FORKSRV_FD + 1) {
                Ok(()) => (),
                Err(_) => {
                    return Err(io::Error::last_os_error());
                }
            }
            unsafe {
                libc::close(st_read);
                libc::close(st_write);
                libc::close(ctl_read);
                libc::close(ctl_write);
            }
            Ok(())
        };
        unsafe { self.pre_exec(func) }
    }

    fn setstdin(&mut self, fd: RawFd, use_stdin: bool) -> &mut Self {
        if use_stdin {
            let func = move || {
                match dup2(fd, libc::STDIN_FILENO) {
                    Ok(()) => (),
                    Err(_) => {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            };
            unsafe { self.pre_exec(func) }
        } else {
            self
        }
    }

    #[allow(trivial_numeric_casts, clippy::cast_possible_wrap)]
    fn setlimit(&mut self, memlimit: u64) -> &mut Self {
        if memlimit == 0 {
            return self;
        }
        // # Safety
        // This method does not do shady pointer foo.
        // It merely call libc functions.
        let func = move || {
            let memlimit: libc::rlim_t = (memlimit as libc::rlim_t) << 20;
            let r = libc::rlimit {
                rlim_cur: memlimit,
                rlim_max: memlimit,
            };
            let r0 = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };

            let mut ret = unsafe { libc::setrlimit(libc::RLIMIT_AS, &r) };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
            ret = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &r0) };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        };
        // # Safety
        // This calls our non-shady function from above.
        unsafe { self.pre_exec(func) }
    }
}

/// The [`Forkserver`] is communication channel with a child process that forks on request of the fuzzer.
/// The communication happens via pipe.
#[derive(Debug)]
pub struct Forkserver {
    /// The "actual" forkserver we spawned in the target
    fsrv_handle: Child,
    /// Status pipe
    st_pipe: Pipe,
    /// Control pipe
    ctl_pipe: Pipe,
    /// Pid of the current forked child (child of the forkserver) during execution
    child_pid: Option<Pid>,
    /// The last status reported to us by the in-target forkserver
    status: i32,
    /// If the last run timed out (in in-target i32)
    last_run_timed_out: i32,
    /// The signal this [`Forkserver`] will use to kill (defaults to [`self.kill_signal`])
    kill_signal: Signal,
}

impl Drop for Forkserver {
    fn drop(&mut self) {
        // Modelled after <https://github.com/AFLplusplus/AFLplusplus/blob/dee76993812fa9b5d8c1b75126129887a10befae/src/afl-forkserver.c#L1429>
        log::debug!("Dropping forkserver",);

        if let Some(pid) = self.child_pid {
            log::debug!("Sending {} to child {pid}", self.kill_signal);
            if let Err(err) = kill(pid, self.kill_signal) {
                log::warn!(
                    "Failed to deliver kill signal to child process {}: {err} ({})",
                    pid,
                    io::Error::last_os_error()
                );
            }
        }

        let forkserver_pid = Pid::from_raw(self.fsrv_handle.id().try_into().unwrap());
        if let Err(err) = kill(forkserver_pid, self.kill_signal) {
            log::warn!(
                "Failed to deliver {} signal to forkserver {}: {err} ({})",
                self.kill_signal,
                forkserver_pid,
                io::Error::last_os_error()
            );
            let _ = kill(forkserver_pid, Signal::SIGKILL);
        } else if let Err(err) = waitpid(forkserver_pid, None) {
            log::warn!(
                "Waitpid on forkserver {} failed: {err} ({})",
                forkserver_pid,
                io::Error::last_os_error()
            );
            let _ = kill(forkserver_pid, Signal::SIGKILL);
        }
    }
}

#[allow(clippy::fn_params_excessive_bools)]
impl Forkserver {
    /// Create a new [`Forkserver`]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: OsString,
        args: Vec<OsString>,
        envs: Vec<(OsString, OsString)>,
        input_filefd: RawFd,
        use_stdin: bool,
        memlimit: u64,
        is_persistent: bool,
        is_deferred_frksrv: bool,
        debug_output: bool,
    ) -> Result<Self, Error> {
        Self::with_kill_signal(
            target,
            args,
            envs,
            input_filefd,
            use_stdin,
            memlimit,
            is_persistent,
            is_deferred_frksrv,
            debug_output,
            KILL_SIGNAL_DEFAULT,
        )
    }

    /// Create a new [`Forkserver`] that will kill child processes
    /// with the given `kill_signal`.
    /// Using `Forkserver::new(..)` will default to [`Signal::SIGTERM`].
    #[allow(clippy::too_many_arguments)]
    pub fn with_kill_signal(
        target: OsString,
        args: Vec<OsString>,
        envs: Vec<(OsString, OsString)>,
        input_filefd: RawFd,
        use_stdin: bool,
        memlimit: u64,
        is_persistent: bool,
        is_deferred_frksrv: bool,
        debug_output: bool,
        kill_signal: Signal,
    ) -> Result<Self, Error> {
        let mut st_pipe = Pipe::new().unwrap();
        let mut ctl_pipe = Pipe::new().unwrap();

        let (stdout, stderr) = if debug_output {
            (Stdio::inherit(), Stdio::inherit())
        } else {
            (Stdio::null(), Stdio::null())
        };

        let mut command = Command::new(target);

        // Setup args, stdio
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);

        // Persistent, deferred forkserver
        if is_persistent {
            command.env("__AFL_PERSISTENT", "1");
        }

        if is_deferred_frksrv {
            command.env("__AFL_DEFER_FORKSRV", "1");
        }

        let fsrv_handle = match command
            .env("LD_BIND_NOW", "1")
            .envs(envs)
            .setlimit(memlimit)
            .setsid()
            .setstdin(input_filefd, use_stdin)
            .setpipe(
                st_pipe.read_end().unwrap(),
                st_pipe.write_end().unwrap(),
                ctl_pipe.read_end().unwrap(),
                ctl_pipe.write_end().unwrap(),
            )
            .spawn()
        {
            Ok(fsrv_handle) => fsrv_handle,
            Err(err) => {
                return Err(Error::illegal_state(format!(
                    "Could not spawn the forkserver: {err:#?}"
                )))
            }
        };

        // Ctl_pipe.read_end and st_pipe.write_end are unnecessary for the parent, so we'll close them
        ctl_pipe.close_read_end();
        st_pipe.close_write_end();

        Ok(Self {
            fsrv_handle,
            st_pipe,
            ctl_pipe,
            child_pid: None,
            status: 0,
            last_run_timed_out: 0,
            kill_signal,
        })
    }

    /// If the last run timed out (as in-target i32)
    #[must_use]
    pub fn last_run_timed_out_raw(&self) -> i32 {
        self.last_run_timed_out
    }

    /// If the last run timed out
    #[must_use]
    pub fn last_run_timed_out(&self) -> bool {
        self.last_run_timed_out_raw() != 0
    }

    /// Sets if the last run timed out (as in-target i32)
    #[inline]
    pub fn set_last_run_timed_out_raw(&mut self, last_run_timed_out: i32) {
        self.last_run_timed_out = last_run_timed_out;
    }

    /// Sets if the last run timed out
    #[inline]
    pub fn set_last_run_timed_out(&mut self, last_run_timed_out: bool) {
        self.last_run_timed_out = i32::from(last_run_timed_out);
    }

    /// The status
    #[must_use]
    pub fn status(&self) -> i32 {
        self.status
    }

    /// Sets the status
    pub fn set_status(&mut self, status: i32) {
        self.status = status;
    }

    /// The child pid
    #[must_use]
    pub fn child_pid(&self) -> Option<Pid> {
        self.child_pid
    }

    /// Set the child pid
    pub fn set_child_pid(&mut self, child_pid: Pid) {
        self.child_pid = Some(child_pid);
    }

    /// Remove the child pid.
    pub fn reset_child_pid(&mut self) {
        self.child_pid = None;
    }

    /// Read from the st pipe
    pub fn read_st(&mut self) -> Result<(usize, i32), Error> {
        let mut buf: [u8; 4] = [0_u8; 4];

        let rlen = self.st_pipe.read(&mut buf)?;
        let val: i32 = i32::from_ne_bytes(buf);
        Ok((rlen, val))
    }

    /// Read bytes of any length from the st pipe
    pub fn read_st_size(&mut self, size: usize) -> Result<(usize, Vec<u8>), Error> {
        let mut buf = vec![0; size];

        let rlen = self.st_pipe.read(&mut buf)?;
        Ok((rlen, buf))
    }

    /// Write to the ctl pipe
    pub fn write_ctl(&mut self, val: i32) -> Result<usize, Error> {
        let slen = self.ctl_pipe.write(&val.to_ne_bytes())?;

        Ok(slen)
    }

    /// Write to the ctl pipe with a timeout
    ///
    /// Returns the number of bytes written
    ///
    /// If the write times out, None is returned.
    pub fn write_ctl_timed(
        &mut self,
        val: i32,
        timeout: &TimeSpec,
    ) -> Result<Option<usize>, Error> {
        let Some(ctrl_write) = self.ctl_pipe.write_end() else {
            return Err(Error::file(io::Error::new(
                ErrorKind::BrokenPipe,
                "Read pipe end was already closed",
            )));
        };

        // # Safety
        // The FDs are valid as this point in time.
        let ctrl_write = unsafe { BorrowedFd::borrow_raw(ctrl_write) };

        let mut writefds = FdSet::new();
        writefds.insert(&ctrl_write);
        let sret = pselect(
            Some(writefds.highest().unwrap().as_raw_fd() + 1),
            None,
            &mut writefds,
            None,
            Some(timeout),
            Some(&SigSet::empty()),
        )?;

        let slen = if sret > 0 {
            Some(self.ctl_pipe.write(&val.to_ne_bytes())?)
        } else {
            None
        };
        Ok(slen)
    }

    /// Read a message from the child process.
    ///
    /// If the read times out, Ok(None) is returned.
    pub fn read_st_timed(&mut self, timeout: &TimeSpec) -> Result<Option<i32>, Error> {
        let mut buf: [u8; 4] = [0_u8; 4];
        let Some(st_read) = self.st_pipe.read_end() else {
            return Err(Error::file(io::Error::new(
                ErrorKind::BrokenPipe,
                "Read pipe end was already closed",
            )));
        };

        // # Safety
        // The FDs are valid as this point in time.
        let st_read = unsafe { BorrowedFd::borrow_raw(st_read) };

        let mut readfds = FdSet::new();
        readfds.insert(&st_read);
        // We'll pass a copied timeout to keep the original timeout intact, because select updates timeout to indicate how much time was left. See select(2)
        let sret = pselect(
            Some(readfds.highest().unwrap().as_raw_fd() + 1),
            &mut readfds,
            None,
            None,
            Some(timeout),
            Some(&SigSet::empty()),
        )?;
        if sret > 0 {
            if self.st_pipe.read_exact(&mut buf).is_ok() {
                let val: i32 = i32::from_ne_bytes(buf);
                Ok(Some(val))
            } else {
                Err(Error::unknown(
                    "Unable to communicate with (read from) fork server (OOM?)".to_string(),
                ))
            }
        } else {
            Ok(None)
        }
    }
}

#[non_exhaustive]
#[derive(Debug, PartialEq)]
enum InputMode {
    Stdin,
    Shmem,
    SocketServer(u16),
    SocketClient(u16),
}

/// This [`Executor`] can run binaries compiled for AFL/AFL++ that make use of a forkserver.
/// Shared memory feature is also available, but you have to set things up in your code.
/// Please refer to AFL++'s docs. <https://github.com/AFLplusplus/AFLplusplus/blob/stable/instrumentation/README.persistent_mode.md>
pub struct ForkserverExecutor<OT, S, SP>
where
    SP: ShMemProvider,
{
    target: OsString,
    args: Vec<OsString>,
    input_mode: InputMode,
    forkserver: Forkserver,
    observers: OT,
    input_file: Option<InputFile>,
    socket_con: Option<SocketConnector>,
    map: Option<SP::ShMem>,
    phantom: PhantomData<S>,
    map_size: Option<usize>,
    timeout: TimeSpec,
    request_response_collector: Option<RequestResponseCollector>,
}

impl<OT, S, SP> Debug for ForkserverExecutor<OT, S, SP>
where
    OT: Debug,
    SP: ShMemProvider,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForkserverExecutor")
            .field("target", &self.target)
            .field("args", &self.args)
            .field("input_file", &self.input_file)
            .field("input_mode", &self.input_mode)
            .field("forkserver", &self.forkserver)
            .field("observers", &self.observers)
            .field("map", &self.map)
            .finish_non_exhaustive()
    }
}

impl ForkserverExecutor<(), (), UnixShMemProvider> {
    /// Builder for `ForkserverExecutor`
    #[must_use]
    pub fn builder() -> ForkserverExecutorBuilder<'static, UnixShMemProvider> {
        ForkserverExecutorBuilder::new()
    }
}

impl<OT, S, SP> ForkserverExecutor<OT, S, SP>
where
    OT: ObserversTuple<S>,
    S: UsesInput,
    SP: ShMemProvider,
{
    /// The `target` binary that's going to run.
    pub fn target(&self) -> &OsString {
        &self.target
    }

    /// The `args` used for the binary.
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// Get a reference to the [`Forkserver`] instance.
    pub fn forkserver(&self) -> &Forkserver {
        &self.forkserver
    }

    /// Get a mutable reference to the [`Forkserver`] instance.
    pub fn forkserver_mut(&mut self) -> &mut Forkserver {
        &mut self.forkserver
    }

    /// The [`InputFile`] used by this [`Executor`], if stdin is used as the input mode.
    pub fn input_file(&self) -> &Option<InputFile> {
        &self.input_file
    }

    /// The coverage map size if specified by the target
    pub fn coverage_map_size(&self) -> Option<usize> {
        self.map_size
    }

    // Drops the forkserver, returning the RequestResponseCollector, enables creating a new forkserver.
    pub fn shutdown(mut self) -> (Option<RequestResponseCollector>, OT) {
        (self.request_response_collector.take(), self.observers)
    }
}

/// The builder for `ForkserverExecutor`
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct ForkserverExecutorBuilder<'a, SP> {
    program: Option<OsString>,
    arguments: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
    debug_child: bool,
    use_stdin: bool,
    uses_shmem_testcase: bool,
    is_persistent: bool,
    is_deferred_frksrv: bool,
    autotokens: Option<&'a mut Tokens>,
    input_filename: Option<OsString>,
    shmem_provider: Option<&'a mut SP>,
    socket_port: Option<u16>,
    socket_client_mode: bool,
    max_input_size: usize,
    map_size: Option<usize>,
    real_map_size: i32,
    kill_signal: Option<Signal>,
    timeout: Option<Duration>,
    request_response_collector: Option<RequestResponseCollector>,
}

impl<'a, SP> ForkserverExecutorBuilder<'a, SP> {
    /// Builds `ForkserverExecutor`.
    /// If a socket port is given, inputs will be provided over that socket.
    /// Else, Forkserver will attempt to provide inputs over shared mem if `shmem_provider` is given.
    /// Else this forkserver will pass the input to the target via `stdin`
    /// in case no input file is specified.
    /// If `debug_child` is set, the child will print to `stdout`/`stderr`.
    #[allow(clippy::pedantic)]
    pub fn build<OT, S>(&mut self, observers: OT) -> Result<ForkserverExecutor<OT, S, SP>, Error>
    where
        OT: ObserversTuple<S>,
        S: UsesInput,
        S::Input: Input + HasTargetBytes,
        SP: ShMemProvider,
    {
        let (forkserver, input_mode, input_file, map) = self.build_helper()?;

        let target = self.program.take().unwrap();
        log::info!(
            "ForkserverExecutor: program: {:?}, arguments: {:?}, use_stdin: {:?}",
            target,
            self.arguments.clone(),
            self.use_stdin
        );

        let socket_con = match input_mode {
            InputMode::SocketServer(port) => Some(SocketConnector::new_server(port)?),
            InputMode::SocketClient(port) => Some(SocketConnector::new_client(port)),
            _ => None,
        };

        if self.uses_shmem_testcase && map.is_none() {
            return Err(Error::illegal_state(
                "Map must always be set for `uses_shmem_testcase`",
            ));
        }

        let timeout: TimeSpec = match self.timeout {
            Some(t) => t.into(),
            None => Duration::from_millis(5000).into(),
        };

        Ok(ForkserverExecutor {
            target,
            args: self.arguments.clone(),
            forkserver,
            observers,
            input_file,
            socket_con,
            map,
            phantom: PhantomData,
            map_size: self.map_size,
            timeout,
            input_mode,
            request_response_collector: self.request_response_collector.take(),
        })
    }

    /// Builds `ForkserverExecutor` downsizing the coverage map to fit exaclty the AFL++ map size.
    ///
    /// If a socket port is given, inputs will be provided over that socket.
    /// Else, Forkserver will attempt to provide inputs over shared mem if `shmem_provider` is given.
    /// Else this forkserver will pass the input to the target via `stdin`
    /// in case no input file is specified.
    /// If `debug_child` is set, the child will print to `stdout`/`stderr`.
    #[allow(clippy::pedantic)]
    pub fn build_dynamic_map<MO, OT, S>(
        &mut self,
        mut map_observer: MO,
        other_observers: OT,
    ) -> Result<ForkserverExecutor<(MO, OT), S, SP>, Error>
    where
        MO: Observer<S> + MapObserver + Truncate, // TODO maybe enforce Entry = u8 for the cov map
        OT: ObserversTuple<S> + Prepend<MO, PreprendResult = OT>,
        S: UsesInput,
        S::Input: Input + HasTargetBytes,
        SP: ShMemProvider,
    {
        let (forkserver, input_mode, input_file, map) = self.build_helper()?;

        let target = self.program.take().unwrap();
        log::info!(
            "ForkserverExecutor: program: {:?}, arguments: {:?}, use_stdin: {:?}, map_size: {:?}",
            target,
            self.arguments.clone(),
            self.use_stdin,
            self.map_size
        );

        let socket_con = if let InputMode::SocketServer(port) = input_mode {
            Some(SocketConnector::new_server(port)?)
        } else {
            None
        };

        if let Some(dynamic_map_size) = self.map_size {
            map_observer.truncate(dynamic_map_size);
        }

        let observers: (MO, OT) = other_observers.prepend(map_observer);

        if self.uses_shmem_testcase && map.is_none() {
            return Err(Error::illegal_state(
                "Map must always be set for `uses_shmem_testcase`",
            ));
        }

        let timeout: TimeSpec = match self.timeout {
            Some(t) => t.into(),
            None => Duration::from_millis(5000).into(),
        };

        Ok(ForkserverExecutor {
            target,
            args: self.arguments.clone(),
            forkserver,
            observers,
            input_file,
            socket_con,
            map,
            phantom: PhantomData,
            map_size: self.map_size,
            timeout,
            input_mode,
            request_response_collector: self.request_response_collector.take(),
        })
    }

    #[allow(clippy::pedantic)]
    fn build_helper(
        &mut self,
    ) -> Result<(Forkserver, InputMode, Option<InputFile>, Option<SP::ShMem>), Error>
    where
        SP: ShMemProvider,
    {
        // deduce input mode
        let input_mode = if let Some(port) = self.socket_port {
            if self.socket_client_mode {
                InputMode::SocketClient(port)
            } else {
                InputMode::SocketServer(port)
            }
        } else if self.shmem_provider.is_some() {
            InputMode::Shmem
        } else {
            InputMode::Stdin
        };

        let input_file = if input_mode == InputMode::Stdin {
            let input_filename = match &self.input_filename {
                Some(name) => name.clone(),
                None => OsString::from(get_unique_std_input_file()),
            };
            Some(InputFile::create(input_filename)?)
        } else {
            None
        };

        let map = match &mut self.shmem_provider {
            None => None,
            Some(provider) => {
                // setup shared memory
                let mut shmem = provider.new_shmem(self.max_input_size + SHMEM_FUZZ_HDR_SIZE)?;
                shmem.write_to_env("__AFL_SHM_FUZZ_ID")?;

                let size_in_bytes = (self.max_input_size + SHMEM_FUZZ_HDR_SIZE).to_ne_bytes();
                shmem.as_mut_slice()[..4].clone_from_slice(&size_in_bytes[..4]);
                Some(shmem)
            }
        };

        let input_fd = input_file
            .as_ref()
            .map(|f| f.as_raw_fd())
            .unwrap_or_default();

        let mut forkserver = match &self.program {
            Some(t) => Forkserver::with_kill_signal(
                t.clone(),
                self.arguments.clone(),
                self.envs.clone(),
                input_fd,
                input_mode == InputMode::Stdin,
                0,
                self.is_persistent,
                self.is_deferred_frksrv,
                self.debug_child,
                self.kill_signal.unwrap_or(KILL_SIGNAL_DEFAULT),
            )?,
            None => {
                return Err(Error::illegal_argument(
                    "ForkserverExecutorBuilder::build: target file not found".to_string(),
                ))
            }
        };

        let (rlen, status) = forkserver.read_st()?; // Initial handshake, read 4-bytes hello message from the forkserver.

        if rlen != 4 {
            return Err(Error::unknown("Failed to start a forkserver".to_string()));
        }
        log::info!("All right - fork server is up.");

        if status & FS_OPT_ENABLED == FS_OPT_ENABLED && status & FS_OPT_MAPSIZE == FS_OPT_MAPSIZE {
            let mut map_size = fs_opt_get_mapsize(status);
            // When 0, we assume that map_size was filled by the user or const
            /* TODO autofill map size from the observer

            if map_size > 0 {
                self.map_size = Some(map_size as usize);
            }
            */

            self.real_map_size = map_size;
            if map_size % 64 != 0 {
                map_size = ((map_size + 63) >> 6) << 6;
            }

            // TODO set AFL_MAP_SIZE
            assert!(self.map_size.is_none() || map_size as usize <= self.map_size.unwrap());

            self.map_size = Some(map_size as usize);
        }

        // Only with SHMEM or AUTODICT we can send send_status back or it breaks!
        // If forkserver is responding, we then check if there's any option enabled.
        // We'll send 4-bytes message back to the forkserver to tell which features to use
        // The forkserver is listening to our response if either shmem fuzzing is enabled or auto dict is enabled
        // <https://github.com/AFLplusplus/AFLplusplus/blob/147654f8715d237fe45c1657c87b2fe36c4db22a/instrumentation/afl-compiler-rt.o.c#L1026>
        if status & FS_OPT_ENABLED == FS_OPT_ENABLED
            && (status & FS_OPT_SHDMEM_FUZZ == FS_OPT_SHDMEM_FUZZ
                || status & FS_OPT_AUTODICT == FS_OPT_AUTODICT)
        {
            let mut send_status = FS_OPT_ENABLED;

            if (status & FS_OPT_SHDMEM_FUZZ == FS_OPT_SHDMEM_FUZZ) && map.is_some() {
                log::info!("Using SHARED MEMORY FUZZING feature.");
                send_status |= FS_OPT_SHDMEM_FUZZ;
                self.uses_shmem_testcase = true;
            }

            if (status & FS_OPT_AUTODICT == FS_OPT_AUTODICT) && self.autotokens.is_some() {
                log::info!("Using AUTODICT feature");
                send_status |= FS_OPT_AUTODICT;
            }

            if send_status != FS_OPT_ENABLED {
                // if send_status is not changed (Options are available but we didn't use any), then don't send the next write_ctl message.
                // This is important

                let send_len = forkserver.write_ctl(send_status)?;
                if send_len != 4 {
                    return Err(Error::unknown("Writing to forkserver failed.".to_string()));
                }

                if (send_status & FS_OPT_AUTODICT) == FS_OPT_AUTODICT {
                    let (read_len, dict_size) = forkserver.read_st()?;
                    if read_len != 4 {
                        return Err(Error::unknown(
                            "Reading from forkserver failed.".to_string(),
                        ));
                    }

                    if !(2..=0xffffff).contains(&dict_size) {
                        return Err(Error::illegal_state(
                            "Dictionary has an illegal size".to_string(),
                        ));
                    }

                    log::info!("Autodict size {dict_size:x}");

                    let (rlen, buf) = forkserver.read_st_size(dict_size as usize)?;

                    if rlen != dict_size as usize {
                        return Err(Error::unknown("Failed to load autodictionary".to_string()));
                    }
                    if let Some(t) = &mut self.autotokens {
                        t.parse_autodict(&buf, dict_size as usize);
                    }
                }
            }
        } else {
            log::warn!("Forkserver Options are not available.");
        }

        Ok((forkserver, input_mode, input_file, map))
    }

    /// Use a socket server to communicate the test cases?
    ///
    /// This means that the target behaves as a client.
    /// If both this method and [`ForkserverExecutorBuilder::socket_client_port`] are called,
    /// the last one is used.
    #[must_use]
    pub fn socket_server_port(mut self, port: u16) -> Self {
        self.socket_port = Some(port);
        self.socket_client_mode = false;
        self
    }

    /// Use a socket client to communicate the test cases?
    ///
    /// This means that the target behaves as a server.
    /// If both this method and [`ForkserverExecutorBuilder::socket_server_port`] are called,
    /// the last one is used.
    #[must_use]
    pub fn socket_client_port(mut self, port: u16) -> Self {
        self.socket_port = Some(port);
        self.socket_client_mode = true;
        self
    }

    /// Use autodict?
    #[must_use]
    pub fn autotokens(mut self, tokens: &'a mut Tokens) -> Self {
        self.autotokens = Some(tokens);
        self
    }

    #[must_use]
    /// set the timeout for the executor
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    #[must_use]
    /// Parse afl style command line
    ///
    /// Replaces `@@` with the path to the input file generated by the fuzzer. If `@@` is omitted,
    /// `stdin` is used to pass the test case instead.
    ///
    /// Interprets the first argument as the path to the program as long as it is not set yet.
    /// You have to omit the program path in case you have set it already. Otherwise
    /// it will be interpreted as a regular argument, leading to probably unintended results.
    pub fn parse_afl_cmdline<IT, O>(self, args: IT) -> Self
    where
        IT: IntoIterator<Item = O>,
        O: AsRef<OsStr>,
    {
        let mut moved = self;

        let mut use_arg_0_as_program = false;
        if moved.program.is_none() {
            use_arg_0_as_program = true;
        }

        for item in args {
            if use_arg_0_as_program {
                moved = moved.program(item);
                // After the program has been set, unset `use_arg_0_as_program` to treat all
                // subsequent arguments as regular arguments
                use_arg_0_as_program = false;
            } else if item.as_ref() == "@@" {
                if let Some(name) = &moved.input_filename.clone() {
                    // If the input file name has been modified, use this one
                    moved = moved.arg_input_file(name);
                } else {
                    moved = moved.arg_input_file_std();
                }
            } else {
                moved = moved.arg(item);
            }
        }

        // If we have not set an input file, use stdin as it is AFLs default
        moved.use_stdin = moved.input_filename.is_none();
        moved
    }

    /// The harness
    #[must_use]
    pub fn program<O>(mut self, program: O) -> Self
    where
        O: AsRef<OsStr>,
    {
        self.program = Some(program.as_ref().to_owned());
        self
    }

    /// Adds an argument to the harness's commandline
    ///
    /// You may want to use `parse_afl_cmdline` if you're going to pass `@@`
    /// represents the input file generated by the fuzzer (similar to the `afl-fuzz` command line).
    #[must_use]
    pub fn arg<O>(mut self, arg: O) -> Self
    where
        O: AsRef<OsStr>,
    {
        self.arguments.push(arg.as_ref().to_owned());
        self
    }

    /// Adds arguments to the harness's commandline
    ///
    /// You may want to use `parse_afl_cmdline` if you're going to pass `@@`
    /// represents the input file generated by the fuzzer (similar to the `afl-fuzz` command line).
    #[must_use]
    pub fn args<IT, O>(mut self, args: IT) -> Self
    where
        IT: IntoIterator<Item = O>,
        O: AsRef<OsStr>,
    {
        let mut res = vec![];
        for arg in args {
            res.push(arg.as_ref().to_owned());
        }
        self.arguments.append(&mut res);
        self
    }

    /// Adds an environmental var to the harness's commandline
    #[must_use]
    pub fn env<K, V>(mut self, key: K, val: V) -> Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.envs
            .push((key.as_ref().to_owned(), val.as_ref().to_owned()));
        self
    }

    /// Adds environmental vars to the harness's commandline
    #[must_use]
    pub fn envs<IT, K, V>(mut self, vars: IT) -> Self
    where
        IT: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let mut res = vec![];
        for (ref key, ref val) in vars {
            res.push((key.as_ref().to_owned(), val.as_ref().to_owned()));
        }
        self.envs.append(&mut res);
        self
    }

    /// Place the input at this position and set the filename for the input.
    ///
    /// Note: If you use this, you should ensure that there is only one instance using this
    /// file at any given time.
    #[must_use]
    pub fn arg_input_file<P: AsRef<Path>>(self, path: P) -> Self {
        let mut moved = self.arg(path.as_ref());

        let path_as_string = path.as_ref().as_os_str().to_os_string();

        assert!(
            // It's only save to set the input_filename, if it does not overwrite an existing one.
            (moved.input_filename.is_none() || moved.input_filename.unwrap() == path_as_string),
            "Already specified an input file under a different name. This is not supported"
        );

        moved.input_filename = Some(path_as_string);
        moved
    }

    /// Place the input at this position and set the default filename for the input.
    #[must_use]
    /// The filename includes the PID of the fuzzer to ensure that no two fuzzers write to the same file
    pub fn arg_input_file_std(self) -> Self {
        self.arg_input_file(get_unique_std_input_file())
    }

    /// If `debug_child` is set, the child will print to `stdout`/`stderr`.
    #[must_use]
    pub fn debug_child(mut self, debug_child: bool) -> Self {
        self.debug_child = debug_child;
        self
    }

    /// Call this if you want to run it under persistent mode; default is false
    #[must_use]
    pub fn is_persistent(mut self, is_persistent: bool) -> Self {
        self.is_persistent = is_persistent;
        self
    }

    /// Call this if the harness uses deferred forkserver mode; default is false
    #[must_use]
    pub fn is_deferred_frksrv(mut self, is_deferred_frksrv: bool) -> Self {
        self.is_deferred_frksrv = is_deferred_frksrv;
        self
    }

    /// Call this to set a defauult const coverage map size
    #[must_use]
    pub fn coverage_map_size(mut self, size: usize) -> Self {
        self.map_size = Some(size);
        self
    }

    /// Call this to set a signal to be used to kill child processes after executions
    #[must_use]
    pub fn kill_signal(mut self, kill_signal: Signal) -> Self {
        self.kill_signal = Some(kill_signal);
        self
    }

    /// Set a request response collector. Only does something with socket based input modes.
    #[must_use]
    pub fn collect_request_response_pairs(
        mut self,
        collector: RequestResponseCollector,
    ) -> Self {
        self.request_response_collector = Some(collector);
        self
    }
}

impl<'a> ForkserverExecutorBuilder<'a, UnixShMemProvider> {
    /// Creates a new `AFL`-style [`ForkserverExecutor`] with the given target, arguments and observers.
    /// This is the builder for `ForkserverExecutor`.
    /// If a socket server or client port was given, inputs will be provided over a socket.
    /// Else, this Forkserver will attempt to provide inputs over shared mem when `shmem_provider` is given.
    /// Else this forkserver will pass the input to the target via `stdin`
    /// in case no input file is specified.
    /// If `debug_child` is set, the child will print to `stdout`/`stderr`.
    #[must_use]
    pub fn new() -> ForkserverExecutorBuilder<'a, UnixShMemProvider> {
        ForkserverExecutorBuilder {
            program: None,
            arguments: vec![],
            envs: vec![],
            debug_child: false,
            use_stdin: false,
            uses_shmem_testcase: false,
            is_persistent: false,
            is_deferred_frksrv: false,
            autotokens: None,
            input_filename: None,
            shmem_provider: None,
            socket_port: None,
            socket_client_mode: false,
            map_size: None,
            real_map_size: 0,
            max_input_size: MAX_INPUT_SIZE_DEFAULT,
            kill_signal: None,
            timeout: None,
            request_response_collector: None,
        }
    }

    /// Shmem provider for forkserver's shared memory testcase feature.
    pub fn shmem_provider<SP: ShMemProvider>(
        self,
        shmem_provider: &'a mut SP,
    ) -> ForkserverExecutorBuilder<'a, SP> {
        ForkserverExecutorBuilder {
            program: self.program,
            arguments: self.arguments,
            envs: self.envs,
            debug_child: self.debug_child,
            use_stdin: self.use_stdin,
            uses_shmem_testcase: self.uses_shmem_testcase,
            is_persistent: self.is_persistent,
            is_deferred_frksrv: self.is_deferred_frksrv,
            autotokens: self.autotokens,
            input_filename: self.input_filename,
            shmem_provider: Some(shmem_provider),
            socket_port: self.socket_port,
            socket_client_mode: self.socket_client_mode,
            map_size: self.map_size,
            real_map_size: self.real_map_size,
            max_input_size: MAX_INPUT_SIZE_DEFAULT,
            kill_signal: None,
            timeout: None,
            request_response_collector: self.request_response_collector,
        }
    }
}

impl<'a> Default for ForkserverExecutorBuilder<'a, UnixShMemProvider> {
    fn default() -> Self {
        Self::new()
    }
}

impl<EM, OT, S, SP, Z> Executor<EM, Z> for ForkserverExecutor<OT, S, SP>
where
    OT: ObserversTuple<S>,
    SP: ShMemProvider,
    S: State + HasExecutions,
    S::Input: HasTargetBytes,
    EM: UsesState<State = S>,
    Z: UsesState<State = S>,
{
    #[inline]
    fn run_target(
        &mut self,
        _fuzzer: &mut Z,
        state: &mut Self::State,
        _mgr: &mut EM,
        input: &Self::Input,
    ) -> Result<ExitKind, Error> {
        *state.executions_mut() += 1;

        let mut exit_kind = ExitKind::Ok;

        let last_run_timed_out = self.forkserver.last_run_timed_out_raw();

        if self.forkserver().child_pid().is_none() {
            // The child is killed for some reason, will start a new trace
            if let Some(ref mut collector) = self.request_response_collector {
                collector.start_new_trace()?;
            }
        }

        match self.input_mode {
            InputMode::Stdin => {
                // # SAFETY:
                // Struct can never be created when input mode is Stdin and input file is none.
                let input_file = unsafe { self.input_file.as_mut().unwrap_unchecked() };
                input_file.write_buf(input.target_bytes().as_slice())?;
            }
            InputMode::Shmem => {
                debug_assert!(
                    self.map.is_some(),
                    "The uses_shmem_testcase() bool can only exist when a map is set"
                );
                // # Safety
                // Struct can never be created when input mode is Shmem and map is none.
                let map = unsafe { self.map.as_mut().unwrap_unchecked() };
                let target_bytes = input.target_bytes();
                let mut size = target_bytes.as_slice().len();
                let max_size = map.len() - SHMEM_FUZZ_HDR_SIZE;
                if size > max_size {
                    // Truncate like AFL++ does
                    size = max_size;
                }
                let size_in_bytes = size.to_ne_bytes();
                // The first four bytes tells the size of the shmem.
                map.as_mut_slice()[..SHMEM_FUZZ_HDR_SIZE]
                    .copy_from_slice(&size_in_bytes[..SHMEM_FUZZ_HDR_SIZE]);
                map.as_mut_slice()[SHMEM_FUZZ_HDR_SIZE..(SHMEM_FUZZ_HDR_SIZE + size)]
                    .copy_from_slice(&target_bytes.as_slice()[..size]);
            }
            InputMode::SocketServer(_) => {
                let child_is_none = self.forkserver().child_pid().is_none();
                // # Safety
                // Struct can never be created when input mode is SocketServ and socket connector is none.
                let socket_con = unsafe { self.socket_con.as_mut().unwrap_unchecked() };
                socket_con.serv_start(child_is_none);

                // Input is actually send after the target starts executing, since it needs to connect to
                // our server socket.
            }
            InputMode::SocketClient(_) => {
                let child_is_none = self.forkserver().child_pid().is_none();
                // # Safety
                // Struct can never be created when input mode is SocketServ and socket connector is none.
                let socket_con = unsafe { self.socket_con.as_mut().unwrap_unchecked() };
                if child_is_none {
                    socket_con.client_reset()?;
                }
            }
        }

        let send_len = self
            .forkserver
            .write_ctl_timed(
                last_run_timed_out,
                &TimeSpec::from_duration(Duration::from_secs(2)),
            )?
            .ok_or_else(|| create_timeout_error("Could not write to forkserver"))?;

        self.forkserver.set_last_run_timed_out(false);

        if send_len != 4 {
            return Err(Error::unknown(
                "Unable to request new process from fork server (OOM?)".to_string(),
            ));
        }

        let pid = self
            .forkserver
            .read_st_timed(&TimeSpec::from_duration(Duration::from_secs(2)))?
            .ok_or_else(|| create_timeout_error("Could not read PID from forkserver"))?;

        if pid <= 0 {
            return Err(Error::unknown(
                "Fork server is misbehaving (OOM?)".to_string(),
            ));
        }

        self.forkserver.set_child_pid(Pid::from_raw(pid));

        // Communicate test case through socket.
        match self.input_mode {
            InputMode::SocketServer(_) => {
                // # Safety
                // Struct can never be created when input mode is SocketServer and socket connector is none.
                let socket_con = unsafe { self.socket_con.as_mut().unwrap_unchecked() };
                let stream = socket_con.serv_finish()?;
                stream.write_all(input.target_bytes().as_slice())?;
            }
            InputMode::SocketClient(_) => {
                // # Safety
                // Struct can never be created when input mode is SocketServer and socket connector is none.
                let socket_con = unsafe { self.socket_con.as_mut().unwrap_unchecked() };
                let stream = socket_con.client_connect()?;
                stream.write_all(input.target_bytes().as_slice())?;
            }
            _ => {}
        }

        // Wait for the test case to execute
        if let Some(status) = self.forkserver.read_st_timed(&self.timeout)? {
            self.forkserver.set_status(status);
            if libc::WIFSIGNALED(self.forkserver().status()) {
                exit_kind = ExitKind::Crash;
            }
        } else {
            self.forkserver.set_last_run_timed_out(true);

            // We need to kill the child in case he has timed out, or we can't get the correct pid in the
            // next call to self.executor.forkserver_mut().read_st()?
            let result = kill(
                self.forkserver().child_pid().unwrap(),
                self.forkserver.kill_signal,
            );
            if let Err(e) = result {
                log::warn!("Error killing child: {}", e);
            }
            if let Some(status) = self
                .forkserver
                .read_st_timed(&TimeSpec::from_duration(Duration::from_secs(2)))?
            {
                self.forkserver.set_status(status);
                exit_kind = ExitKind::Timeout;
            } else {
                return Err(create_timeout_error(
                    "Could not read from forkserver after timeout",
                ));
            }
        }

        // At the end of each run, collect the request response pair if we have a collector
        if let Some(ref mut collector) = self.request_response_collector {
            match self.input_mode {
                InputMode::SocketClient(_) | InputMode::SocketServer(_) => {
                    // # Safety
                    // Struct can never be created when input mode is SocketServer and socket connector is none.
                    let socket_con = unsafe { self.socket_con.as_mut().unwrap_unchecked() };
                    if let Some(ref mut stream) = socket_con.stream {
                        // !! This limits responses to be of 4096 bytes or less!
                        // is that a good size? depends on the target, but should be good most of the time
                        let mut response = vec![0u8; 4096];
                        let input_bytes = input.target_bytes();
                        let pair = match stream.read(&mut response) {
                            Ok(num_bytes) => RequestResponsePair::new(
                                exit_kind,
                                input_bytes.as_slice(),
                                &response[..num_bytes],
                            ),
                            Err(e) => {
                                log::warn!("Could not read response from the target: {e}");
                                RequestResponsePair::new(
                                    exit_kind,
                                    input_bytes.as_slice(),
                                    "LibAFLStar_err".as_bytes(),
                                )
                            }
                        };
                        collector.write_pair(&pair)?
                    }
                }
                _ => {}
            }

            // if it's a crash, save the trace
            if exit_kind == ExitKind::Crash {
                collector.save_this_trace();
            }
        }

        // if the child is stopped (only in persistent mode), the child pid is still valid.
        // In all other cases, the child is terminated, thus we reset it.
        if !libc::WIFSTOPPED(self.forkserver().status()) {
            self.forkserver.reset_child_pid();
        }

        Ok(exit_kind)
    }
}

/// Quick and dirty implementation to create socket connections.
///
/// It can work in 2 modes. Server or client. If this acts as a server, the target should act as a client and vice versa.
/// If [`SocketConnector`] is created using [`SocketConnector::new_server`] it is in server mode, if it is created using
/// [`SocketConnector::new_client`] it is in client mode.
///
/// The dirty part is that you can only call certain methods in certain modes, but nothing is stopping you from using it wrong.
/// In client mode, you should *only* call `client_*` methods.
/// In server mode, you first have to call [`SocketConnector::serv_start`]. If there is no stream, this spins up a thread that starts listening.
/// Afterwards you can call [`SocketConnector::serv_finish`] to obtain the mut ref to the TcpStream. Before calling [`SocketConnector::serv_start`] _again_,
/// you *must* have first called [`SocketConnector::serv_finish`].
struct SocketConnector {
    port: u16,
    listener: Option<TcpListener>,
    stream: Option<TcpStream>,
    handle: Option<JoinHandle<Result<(TcpListener, TcpStream), Error>>>,
}

impl SocketConnector {
    /// Creates a new SocketConnector in server mode.
    ///
    /// You are only allowed to call [`SocketConnector::serv_start`] and [`SocketConnector::serv_finish`].
    /// These calls *MUST* be alternating, starting with a [`SocketConnector::serv_start`]. Calling either method
    /// twice without calling the other will yield bad results, probably a panic.
    pub fn new_server(port: u16) -> Result<Self, Error> {
        let listener = TcpListener::bind(format!("localhost:{port}"))?;

        Ok(Self {
            port,
            listener: Some(listener),
            stream: None,
            handle: None,
        })
    }

    /// Creates a new SocketConnector in client mode.
    ///
    /// You are only allowed to call [`SocketConnector::client_connect`].
    pub fn new_client(port: u16) -> Self {
        Self {
            port,
            listener: None,
            stream: None,
            handle: None,
        }
    }

    /// Start listening using the listener on a new thread.
    fn serv_start_listening(&mut self) {
        let listener = self.listener.take().unwrap();
        let handle = std::thread::spawn(move || -> Result<(TcpListener, TcpStream), Error> {
            let (stream, _) = listener.accept()?;
            Ok((listener, stream))
        });
        self.handle = Some(handle);
    }

    /// ONLY CALL THIS AGAIN, WHEN FIRST HAVING CALLED FINISHED CONNECTING
    ///
    /// Checks if the stream is (still) valid and starts listening on a new thread if not.
    ///
    /// `force`: Always shut down the stream and start listening for a new one.
    pub fn serv_start(&mut self, force: bool) {
        if force {
            let stream_opt = self.stream.take();
            if let Some(stream) = stream_opt {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            self.serv_start_listening();
            return;
        }
        match &self.stream {
            Some(stream) => {
                let stream_err = stream.take_error();
                if stream_err.unwrap().is_some() {
                    // we have a stream, but an error occured ...
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    self.stream.take();
                    self.serv_start_listening();
                }
                // stream is still valid!
            }
            None => self.serv_start_listening(),
        }
    }

    /// ONLY CALL THIS AFTER CALLING START CONNECTING
    ///
    /// Get the stream that was returned by the other thread listening.
    ///
    /// If no connection is ever made, this will block indefinitely, currently.
    pub fn serv_finish(&mut self) -> Result<&mut TcpStream, Error> {
        match &self.handle {
            Some(_) => {
                let handle = self.handle.take().unwrap();
                // maybe only try joining for a while and otherwise give a
                // timeout error.
                let (listener, stream) = handle.join().unwrap().unwrap(); // TODO, maybe handle this error!

                self.listener = Some(listener);
                self.stream = Some(stream);
                Ok(self.stream.as_mut().unwrap())
            }
            None => {
                if let Some(stream) = self.stream.as_mut() {
                    // The previous stream was still valid
                    Ok(stream)
                } else {
                    Err(Error::illegal_state("Something went wrong"))
                }
            }
        }
    }

    /// Reset the stream, if there was any.
    pub fn client_reset(&mut self) -> Result<(), Error> {
        if let Some(stream) = self.stream.take() {
            stream.shutdown(Shutdown::Both)?;
        }
        Ok(())
    }

    /// Returns a mut ref to the stream if it is still valid, otherwise connects to
    /// create a new one.
    /// If the connection fails or is refused, connecting is retried a bunch of times.
    /// If the connection times out, an error is returned.
    pub fn client_connect(&mut self) -> Result<&mut TcpStream, Error> {
        let stream: &mut TcpStream = match self.stream {
            Some(ref stream) if stream.take_error()?.is_none() => {
                // stream is still valid :)
                self.stream.as_mut().unwrap()
            }
            _ => {
                // stream is dead!
                if let Some(stream) = self.stream.take() {
                    let _ = stream.shutdown(Shutdown::Both);
                }

                let sock = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), self.port);
                // make timeout configurable??

                let retries = 20;
                for _ in 0..retries {
                    match TcpStream::connect_timeout(&sock, Duration::from_secs(1)) {
                        Ok(stream) => {
                            // If writing the test case or reading the response takes more than 2 seconds,
                            // something has gone wrong
                            let timeout = Some(Duration::from_secs(2));
                            stream.set_write_timeout(timeout)?;
                            stream.set_read_timeout(timeout)?;
                            self.stream = Some(stream);
                            break;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                            // wait before retrying
                            sleep(Duration::from_millis(25));
                            continue;
                        }
                        Err(e) => Err(e)?,
                    };
                }

                if self.stream.is_none() {
                    return Err(create_timeout_error(format!(
                        "Could not connect to the target through the socket, retried {} times.",
                        retries
                    )));
                }

                self.stream.as_mut().unwrap()
            }
        };
        Ok(stream)
    }
}

impl<OT, S, SP> UsesState for ForkserverExecutor<OT, S, SP>
where
    S: State,
    SP: ShMemProvider,
{
    type State = S;
}

impl<OT, S, SP> UsesObservers for ForkserverExecutor<OT, S, SP>
where
    OT: ObserversTuple<S>,
    S: State,
    SP: ShMemProvider,
{
    type Observers = OT;
}

impl<OT, S, SP> HasObservers for ForkserverExecutor<OT, S, SP>
where
    OT: ObserversTuple<S>,
    S: State,
    SP: ShMemProvider,
{
    #[inline]
    fn observers(&self) -> &OT {
        &self.observers
    }

    #[inline]
    fn observers_mut(&mut self) -> &mut OT {
        &mut self.observers
    }
}
