use std::net::TcpStream;
use ssh2::Session;
use notify::{RecommendedWatcher, RecursiveMode, Result, Watcher, Event, EventKind};
use std::io::Read;
use serde::Deserialize;
use std::fs::File;
use path_abs::{PathAbs, PathOps, PathInfo};
use path_abs::ser::ToStfu8;
use notify::event::{ModifyKind, CreateKind, MetadataKind};
use std::time::{Duration, Instant};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::thread;
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::sync::atomic::{AtomicBool, Ordering};

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

    match establish_connection(config.ssh_endpoint()) {
        Err(err) => println!("{}", err),
        Ok(session) => watch(session, config),
    }
}

fn watch(mut session: Session, config: Config) {
    let (sender, receiver) = mpsc::channel::<Result<Event>>();
    let (tx, rx) = mpsc::channel();

    let mut watcher: RecommendedWatcher = Watcher::new_immediate(move |res| {
        sender.send(res).unwrap()
    }).unwrap();

    for path in &config.paths {
        watcher.watch(path, RecursiveMode::Recursive).unwrap();
    }

    // File is being deleted -> ignore_set -> touch + rm
    // File is being created and exists in ignore_set -> remove from ignore_set + touch

    let change_map: Arc<Mutex<HashMap<PathBuf, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let rm_map = Arc::new(Mutex::new(HashMap::new()));
    let timeout_active = Arc::new(AtomicBool::new(false));

    // TODO: Fix too many file changes -- figure out max limit
    thread::spawn({
        let c_clone = Arc::clone(&change_map);
        let r_clone = Arc::clone(&rm_map);
        let t_clone = Arc::clone(&timeout_active);

        move || {
            let mut ignore_set = HashSet::new();

            for res in rx {
                if !t_clone.load(Ordering::Relaxed) {
                    println!("Spawning timeout..");
                    t_clone.store(true, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(100));

                    let mut c_map = c_clone.lock().unwrap();
                    let mut r_map = r_clone.lock().unwrap();

                    let mut changed = String::new();
                    let mut removed = String::new();

                    let mut skip = true;

                    for (k, v) in c_map.iter() {
                        let k = k.clone();
                        let v = v.clone();

                        if ignore_set.contains(&k) {
                            ignore_set.remove(&k);
                        } else {
                            skip = false;
                            let format = format!("{} ", &v.as_str());
                            changed += &format.as_str();

                            if r_map.contains_key(&k) {
                                removed += &format.as_str();
                                ignore_set.insert(k);
                            }
                        }

                    }

                    if !skip {
                        println!("Executing! c_map: {} r_map: {}", c_map.len(), r_map.len());
                        execute_remote(&mut session, format!("touch -a {} ; rm -f {}", changed, removed));
                    }

                    c_map.clear();
                    r_map.clear();
                    t_clone.store(false, Ordering::Relaxed);
                    println!("Timeout finished!");
                }
            }
        }
    });

    for res in receiver {
        match res {
            Err(err) => println!("watch error: {:?}", err),
            Ok(event) => {
                let path_buf = event.paths.first().unwrap().clone();
                let kind = &event.kind;

                if kind.is_remove() || kind.is_create() || kind.is_modify() {
                    let meta_update = match kind {
                        EventKind::Modify(ModifyKind::Metadata(_)) => true,
                        _ => false,
                    };

                    if !meta_update {
                        let file_name = (&path_buf).file_name().unwrap().to_str().unwrap();

                        let mut c_map = change_map.lock().unwrap();
                        let mut r_map = rm_map.lock().unwrap();

                        if !c_map.contains_key(&path_buf) {
                            println!("1 | {} | change_map doesn't contain", &file_name);
                            if let Some(file) = remote_path(&config, &path_buf) {
                                println!("1 | {} | Adding to change_map", &file_name);
                                &c_map.insert(path_buf.clone(), file.clone());

                                if kind.is_remove() {
                                    println!("2 | {} | Adding to rm_map", &file_name);
                                    &r_map.insert(path_buf.clone(), file.clone());
                                }

                                if !timeout_active.load(Ordering::Relaxed) {
                                    tx.send(true).unwrap();
                                }
                            }
                        }
                    }
                }
            },
        }
    }
}

fn trigger_update(session: &mut Session,
                  change_map: &HashMap<PathBuf, String>,
                  rm_map: &HashMap<PathBuf, String>) {

    let mut changed = String::new();

    for file in change_map.values() {
        changed += format!("{} ", file).as_str();
    }

    let mut command = format!("touch -a {}", changed);

    if !rm_map.is_empty() {
        let mut removed = String::new();

        for file in rm_map.values() {
            removed += format!("{} ", file).as_str();
        }

        command = format!("{} && rm {}", command, removed);
    }

    execute_remote(session, command);
}

fn remote_path(config: &Config, path_buf: &PathBuf) -> Option<String> {
    let path = PathAbs::new_unchecked(path_buf.to_owned()).to_stfu8();

    // Jetbrains IDE temp file
    if !path.ends_with("~") {
        return Some(config.to_remote_path(&path))
    }

    None
}

fn execute_remote(session: &mut Session, command: String) {
    if session.authenticated() {
        let mut channel = session.channel_session().unwrap();
        println!("Remote: {}", command);
        channel.exec(command.as_str()).unwrap();
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
    }).collect();

    config
}
