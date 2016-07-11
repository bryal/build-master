#![feature(process_exec, type_ascription)]

#[macro_use]
extern crate nickel;
extern crate libc;
extern crate getopts;
extern crate rustc_serialize;
extern crate markdown;
extern crate mustache;

use std::sync::{Mutex, Arc};
use std::collections::HashMap;
use std::process::{self, Command, Child};
use std::io::{self, BufRead, BufReader, Error};
use std::{thread, mem, env, fs, path};
use std::sync::mpsc;
use std::os::unix::process::CommandExt;
use nickel::{Nickel, HttpRouter, Request, Response, MiddlewareResult, MediaType};
use nickel::status::StatusCode;
use rustc_serialize::Encodable;

/// Extracts the builder description from the builder script file and parses it as markdown
fn builder_description(builder_name: &str) -> Option<String> {
    let builder_path: path::PathBuf = ["builders", builder_name].iter().collect();

    fs::File::open(&builder_path).ok().map(|f| {
        let desc_md = BufReader::new(f)
            .lines()
            .filter_map(|l| {
                println!("l: {:?}", l);
                l.ok().and_then(|s| {
                                    println!("s: {:?}", s);
                    if s.starts_with("#DESC ") {
                                        println!("s[6..]: {:?}", s[6..].to_string());
                        Some(s[6..].to_string())
                    } else {
                        None
                    }
                })
            })
            .collect::<String>();

        markdown::to_html(&desc_md)
    })
}

/// Render a mustache template and write to a http response
///
/// Workaround for `nickel::Response::render` saving templates and ignoring local changes
fn render<'mw, D, P: ?Sized, T>(resp: Response<'mw, D>, path: &P, data: &T)
                     -> MiddlewareResult<'mw, D>
                     where P: AsRef<path::Path>, T: Encodable {
    let path: &path::Path = path.as_ref();
    match mustache::compile_path(&path) {
        Ok(template) => {
            let mut stream = try!(resp.start());
            match template.render(&mut stream, data) {
                Ok(()) => Ok(nickel::Action::Halt(stream)),
                Err(e) => stream.bail(format!("Problem rendering template: {:?}", e))
            }
        },
        Err(e) => Err(nickel::NickelError::new(resp,
                                           format!("Failed to compile template '{:?}': {:?}",
                                                   path, e),
                                           StatusCode::InternalServerError))
    }
}

#[derive(Clone)]
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

struct Builder {
    child: Child,
    child_out: Output,
    /// The receiver half of a channel between a thread reading from std{out,err}.
    ///
    /// Allows for non-blocking reads
    output_rx: mpsc::Receiver<StdoeLine>,
    builder_name: String,
}

impl Builder {
    /// Spawn a new builder service running the build-script `builder_name`
    ///
    /// # Fails
    /// Returns `None` if no build-script can be found to the given name
    fn new(builder_name: &str) -> Option<Builder> {
        let mut script_path = env::current_dir().expect("Could not get current dir");
        script_path.extend(&["builders", builder_name]);

        let mut cmd = Command::new("sh");
        cmd.arg(&script_path)
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

        let mut child = if let Ok(child) = cmd.spawn() {
            child
       } else {
            return None;
        };

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

        Some(Builder {
            child: child,
            child_out: Output::new(),
            output_rx: rx,
            builder_name: builder_name.to_string(),
        })
    }

    /// Reexecute the build-script of this builder
    fn redeploy(&mut self) -> Result<(), ()> {
        *self = try!(Builder::new(&self.builder_name).ok_or(()));
        Ok(())
    }

    /// Get stdout and stderr of this builder process
    fn get_process_output(&mut self) -> Output {
        let Builder { child_out: ref mut out, output_rx: ref rx, .. } = *self;

        while let Ok(line) = rx.try_recv() {
            if let Ok(s) = line {
                out.out.push_str(&s);
                out.out.push('\n');
            } else if let Err(s) = line {
                out.err.push_str(&s);
                out.err.push('\n');
            }
        }
        out.clone()
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        // Kill all processed in the group led by the child
        // This is required because Child::kill does not kill forked grandchildren
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGINT);
        }
    }
}

/// Extract the builder name from a path of the form `/foo/bar/.../BUILDER?baz=quux&...`
fn builder_name<'req>(req: &'req Request) -> &'req str {
    (req.path_without_query().unwrap().as_ref() : &path::Path).file_name()
                                                              .unwrap()
                                                              .to_str()
                                                              .unwrap()
}

/// A manager of all the different builder services
struct ServersManager {
    builders: Mutex<HashMap<String, Builder>>,
}

impl ServersManager {
    fn new() -> ServersManager {
        ServersManager { builders: Mutex::new(HashMap::new()) }
    }

    fn redeploy(&self, builder_name: &str) -> Result<(), ()> {
        let mut builders = self.builders.lock().unwrap();

        if builders.contains_key(builder_name) {
            builders.get_mut(builder_name).unwrap().redeploy()
        } else {
            Builder::new(builder_name)
                .map(|builder| {
                    builders.insert(builder_name.to_string(), builder);
                })
                .ok_or(())
        }
    }
    
    /// (Re)execute the build-script for the target service
    fn redeploy_handler<'mw, D>(&self,
                                req: &Request,
                                mut resp: Response<'mw, D>)
                                -> MiddlewareResult<'mw, D> {
        let builder_name = builder_name(req);

        if self.redeploy(builder_name).is_ok() {
            resp.set(StatusCode::Ok);
            resp.send("success")
        } else {
            resp.set(StatusCode::NotFound);
            resp.send("failure")
        }
    }

    /// Serve an interface for viewing the output of, or redeploying, a builder
    fn manage_handler<'mw, D>(&self,
                              req: &Request,
                              mut resp: Response<'mw, D>)
                              -> MiddlewareResult<'mw, D> {
        #[derive(RustcEncodable)]
        struct ManagerData<'a> {
            stdout: String,
            stderr: String,
            builder: &'a str,
            success: bool,
            desc: String,
        }

        let builder_name = builder_name(req);
        let mut builders = self.builders.lock().unwrap();

        resp.set(MediaType::Html);

        let mut manager_data = ManagerData {
            stdout: String::new(),
            stderr: String::new(),
            builder: builder_name,
            success: false,
            desc: builder_description(builder_name.as_ref()).unwrap_or(String::new()),
        };

        if let Some(builder) = builders.get_mut(builder_name) {
            let output = builder.get_process_output();

            manager_data.stdout = output.out;
            manager_data.stderr = output.err;
            manager_data.success = true;
        } else {
            resp.set(StatusCode::NotFound);
        }

        render(resp, "ui/manage.mustache", &manager_data)
    }
}

/// Get a `Vec` of the filenames of all builder-scripts in the `builders` dir
///
/// # Fails
///
/// Returns an `io::Error` if the directory could not be read
fn get_builder_names() -> io::Result<Vec<String>> {
    let builder_entries = try!(fs::read_dir("builders"));

    let mut builder_names = Vec::new();
    
    for maybe_entry in builder_entries {
        let builder: fs::DirEntry = match maybe_entry {
            Ok(builder) => builder,
            Err(e) => {
                println!("Error reading dir entry in `builders`: {}", e);
                continue
            }
        };
        match builder.file_name().into_string() {
            Ok(builder_name) => builder_names.push(builder_name),
            Err(invalid) => {
                println!("builder name contained invalid unicode data: {:?}", invalid)
            }
        }
    }
    Ok(builder_names)
}

fn root_handler<'mw, D>(_: &mut Request<D>,
                        mut resp: Response<'mw, D>)
                        -> MiddlewareResult<'mw, D> {
    #[derive(RustcEncodable)]
    struct RootData {
        builders: Vec<String>,
        builders_error: bool,
    }

    let mut root_data = RootData {
        builders: Vec::new(),
        builders_error: false,
    };

    if let Ok(builders) = get_builder_names() {
        root_data.builders.extend(builders);
    } else {
        root_data.builders_error = true;
    }

    resp.set(MediaType::Html);

    render(resp, "ui/index.mustache", &root_data)
}

fn print_usage(program: &str, opts: getopts::Options) {
    println!("{}",
             opts.usage(&format!("Usage: {} DATA_DIR [options]", program)));
}

/// Parsed command-line options
struct Opts {
    data_dir: String,
}

/// Get the program command-line arguments
fn get_opts() -> Opts {
    let args: Vec<String> = env::args().collect();
    let program = args[0].clone();

    let mut opts = getopts::Options::new();
    opts.optflag("h", "help", "print this help menu");

    let matches = opts.parse(&args[1..])
                      .unwrap_or_else(|e| panic!(e.to_string()));
    if matches.opt_present("h") {
        print_usage(&program, opts);
        process::exit(0);
    }

    let data_dir = if !matches.free.is_empty() {
        matches.free[0].clone()
    } else {
        print_usage(&program, opts);
        process::exit(0);
    };

    Opts { data_dir: data_dir }
}

fn main() {
    let opts = get_opts();

    env::set_current_dir(&opts.data_dir).unwrap_or_else(|e| panic!("{}", e));

    let mut server = Nickel::new();

    let manager = Arc::new(ServersManager::new());

    let builders = get_builder_names()
                       .unwrap_or_else(|e| panic!("Failed to read `builders` dir: {}", e));

    for builder in builders {
        manager.redeploy(&builder).unwrap();
    }

    let manager_clone = manager.clone();

    server.get("/builder/*",
               middleware!{ |req, resp|
                   return manager_clone.manage_handler(req, resp)
               });
    server.post("/builder/*",
                middleware!{ |req, resp|
                    return manager.redeploy_handler(req, resp)
                });
    server.get("/", root_handler);

    let ip = "0.0.0.0:8016";
    server.listen(ip);
}
