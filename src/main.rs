#![feature(process_exec)]

#[macro_use]
extern crate nickel;
extern crate libc;
extern crate getopts;

use std::sync::{Mutex, Arc};
use std::collections::HashMap;
use std::process::{self, Command, Child};
use std::io::{BufRead, BufReader, Error};
use std::{thread, mem, env};
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
        let mut script_path = env::current_dir().expect("Could not get current dir");
        script_path.push("build-scripts/npm.sh");

        let mut cmd = Command::new("sh");

        cmd.arg(&script_path)
           .current_dir("/home/bryal/hax/drustse")
           .stdout(process::Stdio::piped())
           .stderr(process::Stdio::piped())
           .before_exec(|| {
               // Make this process the leader of its own group.
               // Allows for killing of forked grandchildren
               if unsafe { libc::setsid() } != -1 {
                   Ok(())
               } else {
                   Err(Error::last_os_error())
               }
           });

        let mut child = cmd.spawn().expect("Build script execution failed");

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

impl Drop for BuildServerInner {
    fn drop(&mut self) {
        // Kill all processed in the group led by the child
        // This is required because Child::kill does not kill forked grandchildren
        println!("DROP");
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGINT);
        }
    }
}

/// A http handler that manages the building and deployment of services
struct BuildServer(Mutex<BuildServerInner>);

impl BuildServer {
    fn new() -> BuildServer {
        BuildServer(Mutex::new(BuildServerInner::new()))
    }

    fn redeploy<'mw, D>(&self, mut resp: Response<'mw, D>) -> MiddlewareResult<'mw, D> {
        *self.0.lock().unwrap() = BuildServerInner::new();

        resp.send("success")
    }

    /// Get the current output by the child process, both stdout and stderr
    fn child_output<'mw, D>(&self, mut resp: Response<'mw, D>) -> MiddlewareResult<'mw, D> {
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
        resp.render("ui/output.tpl", &data)
    }
}

fn root_handler<'mw, D>(_: &mut Request<D>,
                        mut resp: Response<'mw, D>)
                        -> MiddlewareResult<'mw, D> {
    resp.set(MediaType::Html);
    resp.render("ui/index.tpl", &HashMap::<&str, &str>::new())
}

fn print_usage(program: &str, opts: getopts::Options) {
    println!("{}",
             opts.usage(&format!("Usage: {} DATA_DIR [options]", program)));
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let program = args[0].clone();

    let mut opts = getopts::Options::new();
    opts.optflag("h", "help", "print this help menu");

    let matches = opts.parse(&args[1..])
                      .unwrap_or_else(|e| panic!(e.to_string()));
    if matches.opt_present("h") {
        print_usage(&program, opts);
        return;
    }

    let data_dir = if !matches.free.is_empty() {
        matches.free[0].clone()
    } else {
        print_usage(&program, opts);
        return;
    };

    env::set_current_dir(&data_dir).unwrap_or_else(|e| panic!("{}", e));

    let mut server = Nickel::new();

    let build_server = Arc::new(BuildServer::new());

    let clone = build_server.clone();
    server.get("/master",
               middleware!{ |_, resp|
                   return clone.child_output(resp)
               });
    server.post("/master",
                middleware!{ |_, resp|
                    return build_server.redeploy(resp)
                });
    server.get("/", root_handler);

    let ip = "0.0.0.0:8016";
    server.listen(ip);
}
