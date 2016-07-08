#[macro_use]
extern crate nickel;

use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::process::Command;
use nickel::{Nickel, HttpRouter, Request, Response, MiddlewareResult, MediaType};

fn root_handler<'res, D>(_: &mut Request<D>,
                         mut res: Response<'res, D>)
                         -> MiddlewareResult<'res, D> {
    res.set(MediaType::Html);
    res.render("ui/index.tpl", &())
}

fn main() {
    let mut server = Nickel::new();

    let mut redeploy = Command::new("cargo");
    redeploy.arg("run");
    redeploy.current_dir("D:/Dropbox/Program/drust");

    let redeploy = Mutex::new(redeploy);

    let child = redeploy.lock().unwrap().spawn().unwrap();

    let child = Mutex::new(child);

    server.get("/redeploy",
               middleware! { |_, res|
                                 let mut child = child.lock().unwrap();

                                 child.kill();

                                 *child = redeploy.lock().unwrap().spawn().unwrap();

                                 return res.send("redeployed!") });

    server.get("/", root_handler);

    let srv = "127.0.0.1:8016";

    server.listen(srv);

    println!("Listening on {}", srv);
}
