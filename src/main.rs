use std::{
    collections::VecDeque,
    io::{self, Read},
    process::ExitStatus,
};

use duct::{Expression, Handle};
use mio::{unix::pipe::Receiver, Events, Interest, Token};

const PYTHON: &str = r#"\
import sys
import time
for i in range(5):
    print("stdout", i, file=sys.stdout)
    print("stderr", i, file=sys.stderr)
    time.sleep(1)
print("done")
"#;

const STDOUT: Token = Token(0);
const STDERR: Token = Token(1);

const BUFFER_SIZE: usize = 9;

#[derive(Clone, Debug)]
enum Out {
    Stdout(String),
    Stderr(String),
    Done(ExitStatus),
}

#[derive(Clone, Copy, Debug)]
enum Stream {
    Stdout,
    Stderr,
}

struct ProcessReader {
    child: Handle,

    stdout_read: Receiver,
    stderr_read: Receiver,

    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    output_buf: VecDeque<Out>,

    poll: mio::Poll,
    events: mio::Events,
    done: bool,
}

impl ProcessReader {
    pub fn start(expr: Expression) -> Result<Self, io::Error> {
        let (stdout_write, mut stdout_read) = mio::unix::pipe::new()?;
        let (stderr_write, mut stderr_read) = mio::unix::pipe::new()?;

        let child = expr
            .stdout_file(stdout_write)
            .stderr_file(stderr_write)
            .start()?;

        let poll = mio::Poll::new()?;
        let events = Events::with_capacity(128);

        poll.registry()
            .register(&mut stdout_read, STDOUT, Interest::READABLE)?;
        poll.registry()
            .register(&mut stderr_read, STDERR, Interest::READABLE)?;

        let stdout_buf = Vec::<u8>::new();
        let stderr_buf = Vec::<u8>::new();
        let output_buf = VecDeque::<Out>::new();

        Ok(Self {
            child,
            stdout_read,
            stderr_read,

            stdout_buf,
            stderr_buf,
            output_buf,

            poll,
            events,
            done: false,
        })
    }
}

fn read_pipe(
    reader: &mut Receiver,
    str_buf: &mut Vec<u8>,
    out_buf: &mut VecDeque<Out>,
    which: Stream,
) -> Result<(), io::Error> {
    loop {
        let mut buf = [0; BUFFER_SIZE];
        let n = match reader.read(&mut buf[..]) {
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                return Ok(());
            }
            Ok(n) => Ok(n),
            err => err,
        }?;

        if n == 0 {
            return Ok(());
        }

        for i in 0..n {
            if buf[i] == b'\n' {
                let line = String::from_utf8_lossy(&str_buf[..]).to_string();
                match which {
                    Stream::Stdout => out_buf.push_back(Out::Stdout(line)),
                    Stream::Stderr => out_buf.push_back(Out::Stderr(line)),
                };

                str_buf.clear();
                continue;
            }

            if buf[i] == b'\r' {
                continue;
            }

            str_buf.push(buf[i]);
        }
    }
}

impl Iterator for ProcessReader {
    type Item = Result<Out, io::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        loop {
            if let Some(next) = self.output_buf.pop_front() {
                return Some(Ok(next));
            }

            match self.poll.poll(&mut self.events, None) {
                Err(err) => return Some(Err(err)),
                _ => {}
            };

            for event in self.events.iter() {
                match event.token() {
                    STDOUT => match read_pipe(
                        &mut self.stdout_read,
                        &mut self.stdout_buf,
                        &mut self.output_buf,
                        Stream::Stdout,
                    ) {
                        Err(err) => return Some(Err(err)),
                        _ => {}
                    },
                    STDERR => match read_pipe(
                        &mut self.stderr_read,
                        &mut self.stderr_buf,
                        &mut self.output_buf,
                        Stream::Stderr,
                    ) {
                        Err(err) => return Some(Err(err)),
                        _ => {}
                    },
                    _ => unreachable!(),
                }
            }

            if !self.output_buf.is_empty() {
                continue;
            }

            match self.child.try_wait() {
                Ok(Some(output)) => {
                    self.done = true;
                    return Some(Ok(Out::Done(output.status)));
                }
                Ok(None) => continue,
                Err(err) => return Some(Err(err)),
            };
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let expr = duct::cmd!("python3", "-u", "-c", PYTHON);
    let reader = ProcessReader::start(expr)?;

    for line in reader {
        println!("{line:?}");
    }

    Ok(())
}
