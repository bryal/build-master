#![feature(process_exec)]

#[macro_use]
extern crate nickel;
extern crate libc;

use std::sync::Mutex;
use std::collections::HashMap;
use std::process::{self, Command, Child};
use std::io::{BufRead, BufReader, Error};
use std::{thread, mem};
use std::sync::mpsc;
use std::os::unix::process::CommandExt;
use nickel::{Nickel, HttpRouter, Request, Response, Middleware, MiddlewareResult, MediaType,
             QueryString};

struct Output {
    out: String,
    err: String,
}

impl Output {
    fn new() -> Output {
        Output {
            out: String::new(),
            err: String::new(),
        }
    }
}

/// An output line from either stdout or stderr
type StdoeLine = Result<String, String>;

struct BuildServerInner {
    child: Child,
    child_out: Output,
    /// The receiver half of a channel between a thread reading from std{out,err}.
    ///
    /// Allows for non-blocking reads
    output_rx: mpsc::Receiver<StdoeLine>,
}

impl BuildServerInner {
    fn new() -> BuildServerInner {
        let mut cmd = Command::new("bash");
        cmd.arg("/home/bryal/hax/build-master/build-scripts/npm.sh")
           .current_dir("/home/bryal/hax/drustse")
           .stdout(process::Stdio::piped())
           .stderr(process::Stdio::piped())
            // Make the spawned child a leader of its own process group.
            // Allows for easy killing of forked grandchildren
            .before_exec(|| if unsafe { libc::setsid() } != -1 {
                Ok(())
            } else {
                Err(Error::last_os_error())
            });

        let mut child = cmd.spawn().expect("Could not execute build command");

        let (child_stdout, child_stderr) = (mem::replace(&mut child.stdout, None).unwrap(),
                                            mem::replace(&mut child.stderr, None).unwrap());
        let (tx, rx) = mpsc::channel();

        // Spawn output reader in own thread and communicate with channels to prevent blocking
        thread::spawn(move || {
            let stdout_lines = BufReader::new(child_stdout)
                .lines()
                .map(|l| l.map(Ok));
            let stderr_lines = BufReader::new(child_stderr).lines().map(|l| l.map(Err));

            for line in stdout_lines.chain(stderr_lines) {
                if let Ok(line) = line {
                    if tx.send(line).is_err() {
                        return;
                    }
                } else {
                    return;
                }
            }
        });

        BuildServerInner {
            child: child,
            child_out: Output::new(),
            output_rx: rx,
        }
    }
}

/// A http handler that manages the building and deployment of services
struct BuildServer(Mutex<BuildServerInner>);

impl BuildServer {
    fn new() -> BuildServer {
        BuildServer(Mutex::new(BuildServerInner::new()))
    }

    fn redeploy(&self) {
        let mut self_ = self.0.lock().unwrap();

        // Kill all processed in the group led by the child
        // This is required because Child::kill does not kill forked grandchildren
        unsafe {
            libc::kill(-(self_.child.id() as i32), libc::SIGINT);
        }

        *self_ = BuildServerInner::new();
    }

    /// Get the current output by the child process, both stdout and stderr
    fn output<'mw, D>(&'mw self, mut resp: Response<'mw, D>) -> MiddlewareResult<'mw, D> {
        let BuildServerInner { child_out: ref mut out, output_rx: ref rx, .. } = *self.0
            .lock()
            .unwrap();

        while let Ok(line) = rx.try_recv() {
            if let Ok(s) = line {
                out.out.push_str(&s);
                out.out.push('\n');
            } else if let Err(s) = line {
                out.err.push_str(&s);
                out.err.push('\n');
            }
        }

        let mut data = HashMap::new();
        data.insert("stdout", &out.out);
        data.insert("stderr", &out.err);

        resp.set(MediaType::Html);
        resp.render("/home/bryal/hax/build-master/ui/output.tpl", &data)
    }
}

impl<D> Middleware<D> for BuildServer {
    fn invoke<'mw>(&'mw self,
                   req: &mut Request<D>,
                   resp: Response<'mw, D>)
                   -> MiddlewareResult<'mw, D> {
        match req.query().get("action") {
            Some("redeploy") => {
                self.redeploy();
                resp.send("Redeployed!")
            }
            _ => self.output(resp),
        }

    }
}

fn root_handler<'mw, D>(_: &mut Request<D>,
                        mut resp: Response<'mw, D>)
                        -> MiddlewareResult<'mw, D> {
    resp.set(MediaType::Html);
    resp.render("/home/bryal/hax/build-master/ui/index.tpl",
                &HashMap::<&str, &str>::new())
}

fn main() {
    let mut server = Nickel::new();

    let build_server = BuildServer::new();

    server.get("/master", build_server);

    server.get("/", root_handler);

    let srv = "0.0.0.0:8016";

    server.listen(srv);

    println!("Listening on {}", srv);
}
