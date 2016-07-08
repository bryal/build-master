#[macro_use]
extern crate nickel;

use std::sync::Mutex;
use std::collections::HashMap;
use std::process::{self, Command, Child};
use std::io::{BufRead, BufReader};
use std::{thread, mem};
use std::sync::mpsc;
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
    redeploy_cmd: Command,
    child: Child,
    child_out: Output,
    /// The receiver half of a channel between a thread reading from std{out,err}.
    ///
    /// Allows for non-blocking reads
    output_rx: mpsc::Receiver<StdoeLine>,
}

/// A http handler that manages the building and deployment of services
struct BuildServer(Mutex<BuildServerInner>);

impl BuildServer {
    fn new() -> BuildServer {
        let mut cmd = Command::new("sh");
        cmd.arg("~/hax/build-master/build-scripts/npm.sh")
           .current_dir("~/hax/drustse")
           .stdout(process::Stdio::piped())
           .stderr(process::Stdio::piped());

        let mut child = cmd.spawn().unwrap();

        // let (child_stdout, child_stderr) = (mem::replace(&mut child.stdout, None).unwrap(),
        //                                     mem::replace(&mut child.stderr, None).unwrap());
        let (tx, rx) = mpsc::channel();

        // Spawn output reader in own thread and communicate with channels to prevent blocking
        // thread::spawn(move || {
        //     let stdout_lines = BufReader::new(child_stdout)
        //                            .lines()
        //                            .map(|l| Ok(l.unwrap()));
        //     let stderr_lines = BufReader::new(child_stderr).lines().map(|l| Err(l.unwrap()));

        //     for line in stdout_lines.chain(stderr_lines) {
        //         println!("line: {}", line.clone().unwrap());
        //         tx.send(line);
        //     }
        // });

        BuildServer(Mutex::new(BuildServerInner {
            redeploy_cmd: cmd,
            child: child,
            child_out: Output::new(),
            output_rx: rx,
        }))
    }

    fn redeploy(&self) {
        let mut self_ = self.0.lock().unwrap();

        self_.child.kill();
        self_.child_out.out.clear();
        self_.child_out.err.clear();

        self_.child = self_.redeploy_cmd.spawn().unwrap();
    }

    /// Get the current output by the child process, both stdout and stderr
    fn output<'mw, D>(&'mw self, mut resp: Response<'mw, D>) -> MiddlewareResult<'mw, D> {
        let BuildServerInner { child_out: ref mut out,
                               output_rx: ref rx,
                               ..} = *self.0
                                          .lock()
                                          .unwrap();

        // while let Ok(line) = rx.try_recv() {
        //     if let Ok(s) = line {
        //         out.out.push_str(&s);
        //         out.out.push('\n');
        //     } else if let Err(s) = line {
        //         out.err.push_str(&s);
        //         out.err.push('\n');
        //     }
        // }

        let mut data = HashMap::new();
        data.insert("stdout", &out.out);
        data.insert("stderr", &out.err);

        resp.set(MediaType::Html);
        resp.render("~/hax/build-master/ui/output.tpl", &data)
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
    resp.render("~/hax/build-master/ui/index.tpl",
                &HashMap::<&str, &str>::new())
}

fn main() {
    let mut server = Nickel::new();

    let build_server = BuildServer::new();

    server.get("/master", build_server);

    server.get("/", root_handler);

    let srv = "127.0.0.1:8016";

    server.listen(srv);

    println!("Listening on {}", srv);
}
