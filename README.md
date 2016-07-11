# build-master

Application for automatically updating, building, and running services. Useful during development when there is an interest in having visible results after each code push


Basically this server just listens for github webhook deliveries, kills the previous incarnation, and runs a build&run script, which should make a git pull

# Usage

1. Install [`rust`](https://www.rust-lang.org)

2. Run `cargo build [--release] && target/{debug,release}/build-master DATA_DIR`

   or

   `cargo run [--release] -- DATA_DIR`

   where `DATA_DIR` is the path to the directory that contains the server data, i.e. the `ui` dir, the `build-script` dir, etc.
