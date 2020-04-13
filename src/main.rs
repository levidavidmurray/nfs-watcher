use std::net::TcpStream;
use ssh2::Session;
use notify::{RecommendedWatcher, RecursiveMode, Result, Watcher, Event, EventKind};
use std::path::{PathBuf};
use std::io::Read;
use serde::Deserialize;
use std::fs::File;
use path_abs::{PathAbs, PathDir, PathOps};
use path_abs::ser::ToStfu8;
use notify::event::ModifyKind;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerConfig {
    host: String,
    port: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    server: ServerConfig,
    local_root: String,
    remote_root: String,
    paths: Vec<String>,
}

impl Config {
    fn ssh_endpoint(&self) -> String {
        format!("{}:{}", self.server.host, self.server.port)
    }

    fn to_remote_path(&self, local_path: &String) -> String {
        local_path.replace(self.local_root.as_str(), self.remote_root.as_str())
    }
}

fn main() {
    let config = get_config();
    println!("{:?}", config);

    match establish_connection(config.ssh_endpoint()) {
        Err(err) => println!("{}", err),
        Ok(session) => watch(session, config),
    }
}

fn watch(mut session: Session, config: Config) {
    let (sender, receiver) = std::sync::mpsc::channel::<Result<Event>>();

    let mut watcher: RecommendedWatcher = Watcher::new_immediate(move |res| {
        sender.send(res).unwrap()
    }).unwrap();

    for path in &config.paths {
        watcher.watch(path, RecursiveMode::Recursive).unwrap();
    }

    // TODO: Timeout on file changes
    for res in receiver {
        match res {
            Ok(event) => handle_event(&mut session, event, &config),
            Err(err) => println!("watch error: {:?}", err),
        }
    }
}

fn handle_event(session: &mut Session, event: Event, config: &Config) {
    match event.kind {
        EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)) => {
            let path_buf = event.paths.first().unwrap();
            let path = PathAbs::new_unchecked(path_buf.to_owned()).to_stfu8();

            if !path.ends_with("~") {
                let remote_path = config.to_remote_path(&path);
                println!("Updating {}: {}", path, remote_path);

                execute_remote(session, format!("touch -a {}", remote_path));
            }

        },
        _ => (),
    }
}

fn execute_remote(session: &mut Session, command: String) {
    if session.authenticated() {
        let mut channel = session.channel_session().unwrap();
        channel.exec(command.as_str()).unwrap();
        let mut s = String::new();
        channel.read_to_string(&mut s).unwrap();
        println!("{}", s);
        channel.wait_close().unwrap();
    }

}

fn establish_connection(addr: String) -> std::result::Result<Session, String> {
    let tcp = TcpStream::connect(addr.to_owned()).unwrap();
    let mut session = Session::new().unwrap();
    let mut agent = session.agent().unwrap();

    agent.connect().unwrap();
    agent.list_identities().unwrap();

    session.set_tcp_stream(tcp);
    session.handshake().unwrap();

    for identity in agent.identities().unwrap() {
        println!("Agent '{}' attempt", identity.comment());

        if agent.userauth("myhealthaccess", &identity).is_ok() {
            return Ok(session);
        }
    }

    Err(format!("Failed to connect to {}", addr))
}

fn get_config() -> Config {
    let mut file = File::open("watcher.json").unwrap();
    let mut buff = String::new();
    file.read_to_string(&mut buff).unwrap();

    let mut config: Config = serde_json::from_str(&buff).unwrap();
    let paths = config.paths.clone();

    let local_root = &PathAbs::new(&config.local_root).unwrap();
    config.local_root = local_root.to_stfu8();
    config.paths = paths.into_iter().map(|dir| {
        PathAbs::new(local_root.join(dir)).unwrap().to_stfu8()
//        PathAbs::from(&local_root(dir)).to_stfu8()
    }).collect();

    config
}

//fn join_paths(path1: &String, path2: String) -> String {
//    get_path_string(&PathBuf::from(path1).join(PathBuf::from(path2)))
//}

//fn get_path_string(path: &PathBuf) -> String {
//    path.canonicalize()
//        .unwrap()
//        .into_os_string()
//        .into_string()
//        .unwrap()
//}
