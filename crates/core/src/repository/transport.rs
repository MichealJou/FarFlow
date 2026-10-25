use super::storage;
use crate::{Error, Result};
use process_wrap::std::CommandWrap;
use std::{
    io::Read,
    process::{Command, Output},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct GitTransport {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl Default for GitTransport {
    fn default() -> Self {
        Self::new(Duration::from_secs(120)).expect("默认 Git 超时时间有效")
    }
}

impl GitTransport {
    pub fn new(timeout: Duration) -> Result<Self> {
        if timeout.is_zero() || timeout > Duration::from_secs(600) {
            return Err(Error::Validation(
                "Git 同步超时需大于零且不超过十分钟".into(),
            ));
        }
        Ok(Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + timeout,
        })
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(Error::Storage(
                "Git 同步已取消，本机记录已保留；若已开始上传，请重试同步核对远端结果".into(),
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(Error::Storage(
                "Git 同步已超时，本机记录已保留；若已开始上传，请重试同步核对远端结果".into(),
            ));
        }
        Ok(())
    }
}

pub(super) fn run(process: Command, control: &GitTransport) -> Result<Output> {
    control.check()?;
    let mut process = CommandWrap::from(process);
    #[cfg(unix)]
    process.wrap(process_wrap::std::ProcessGroup::leader());
    #[cfg(windows)]
    process.wrap(process_wrap::std::JobObject);
    let mut child = process.spawn().map_err(storage)?;
    let stdout = child.stdout().take().expect("Git 网络标准输出已配置管道");
    let stderr = child.stderr().take().expect("Git 网络错误输出已配置管道");
    std::thread::scope(|scope| {
        let stdout = scope.spawn(move || read(stdout));
        let stderr = scope.spawn(move || read(stderr));
        let result = loop {
            if let Err(error) = control.check() {
                break Err(error);
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Err(error) => break Err(storage(error)),
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        // 传输子进程可能继承管道；结束整个组后再等待读取线程，取消不能卡在管道 EOF。
        if result.is_err() || !stdout.is_finished() || !stderr.is_finished() {
            let _ = child.start_kill();
        }
        let _ = child.wait();
        let stdout = stdout
            .join()
            .map_err(|_| Error::Storage("Git 输出读取中断".into()));
        let stderr = stderr
            .join()
            .map_err(|_| Error::Storage("Git 输出读取中断".into()));
        Ok(Output {
            status: result?,
            stdout: stdout??,
            stderr: stderr??,
        })
    })
}

fn read(source: impl Read) -> Result<Vec<u8>> {
    let mut bytes = vec![];
    source
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(storage)?;
    if bytes.len() > 1024 * 1024 {
        return Err(Error::Storage("Git 网络诊断输出超过大小限制".into()));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{self, Cursor},
        process::Stdio,
    };

    fn python(code: &str) -> Command {
        let mut command = Command::new("python3");
        command
            .args(["-c", code])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    #[test]
    fn cancellation_is_shared_and_deadlines_and_duration_limits_are_enforced() {
        assert!(GitTransport::new(Duration::ZERO).is_err());
        assert!(GitTransport::new(Duration::from_secs(601)).is_err());
        let control = GitTransport::default();
        assert!(control.check().is_ok());
        control.clone().cancel();
        control.cancel();
        assert!(control.check().unwrap_err().to_string().contains("已取消"));
        assert!(GitTransport::default().check().is_ok());
        assert!(GitTransport::new(Duration::from_secs(600)).is_ok());
        let control = GitTransport::new(Duration::from_millis(1)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(control.check().unwrap_err().to_string().contains("已超时"));
    }

    #[test]
    fn diagnostic_reads_are_bounded_and_propagate_io_errors() {
        assert!(read(Cursor::new([])).unwrap().is_empty());
        assert_eq!(
            read(Cursor::new(vec![0; 1024 * 1024])).unwrap().len(),
            1024 * 1024
        );
        assert!(read(Cursor::new(vec![0; 1024 * 1024 + 1]))
            .unwrap_err()
            .to_string()
            .contains("大小限制"));
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("受控读取失败"))
            }
        }
        assert!(read(Broken).is_err());
    }

    #[test]
    fn processes_keep_exact_outputs_and_failures_without_unbounded_pipe_reads() {
        let result = run(
            python(
                "import sys;sys.stdout.write('原始输出');sys.stderr.write('诊断输出');sys.exit(42)",
            ),
            &GitTransport::default(),
        )
        .unwrap();
        assert_eq!(result.stdout, "原始输出".as_bytes());
        assert_eq!(result.stderr, "诊断输出".as_bytes());
        assert_eq!(result.status.code(), Some(42));
        assert!(run(
            Command::new("farflow-nonexistent-executable"),
            &GitTransport::default()
        )
        .is_err());
        assert!(run(
            python("import sys;sys.stdout.write('x'*2097152)"),
            &GitTransport::default()
        )
        .unwrap_err()
        .to_string()
        .contains("大小限制"));
        let cancelled = GitTransport::default();
        cancelled.cancel();
        assert!(run(python("raise Exception('不应运行')"), &cancelled)
            .unwrap_err()
            .to_string()
            .contains("已取消"));
        let control = GitTransport::new(Duration::from_millis(100)).unwrap();
        let before = Instant::now();
        assert!(run(python("import time;time.sleep(10)"), &control)
            .unwrap_err()
            .to_string()
            .contains("已超时"));
        assert!(before.elapsed() < Duration::from_secs(2));
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_terminates_inherited_pipe_children_and_returns_promptly() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("child.pid");
        let script = format!("import subprocess,time,pathlib\np=subprocess.Popen(['python3','-c','import time; time.sleep(30)'])\npathlib.Path({pid_path:?}).write_text(str(p.pid))\ntime.sleep(30)");
        let control = GitTransport::default();
        std::thread::scope(|scope| {
            let task = scope.spawn(|| run(python(&script), &control));
            let until = Instant::now() + Duration::from_secs(3);
            while !pid_path.exists() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            control.cancel();
            assert!(task
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("已取消"));
        });
        let pid = std::fs::read_to_string(pid_path).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let running = Command::new("kill")
                .args(["-0", pid.trim()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            if !running {
                break;
            }
            assert!(Instant::now() < until, "取消后传输子进程仍存活");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
