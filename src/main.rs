mod db;
mod discovery;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{
    AuthReply, AuthRequest, ChatMessage, ClientMsg, DirectMessage, ServerMsg, UserStatus, DEFAULT_ADDR,
    DEFAULT_AUTH_ADDR,
    DISCOVERY_PORT, MAX_SERVER_DESCRIPTION_LEN, MAX_SERVER_ICON_LEN,
};
use db::Db;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

const MAX_TEXT_LEN: usize = 2000;
const MAX_SERVER_NAME_LEN: usize = 40;
const MAX_CATEGORY_NAME_LEN: usize = 32;
const DEFAULT_DB_PATH: &str = "vaxelium.db";
/// How long a session confirmed by the auth service keeps working on this server while the auth
/// service cannot be reached.
const SESSION_CACHE_SECS: u64 = 24 * 60 * 60;

struct State {
    db: Db,
    /// Logged-in users, one entry per connection.
    online: Vec<String>,
    /// The address other servers use to reach this one.
    advertise: String,
    /// How many times in a row each known server has failed to answer.
    peer_failures: HashMap<String, u32>,
}

impl State {
    fn remove_online(&mut self, user: &str) {
        if let Some(pos) = self.online.iter().position(|u| u == user) {
            self.online.remove(pos);
        }
    }

    /// Every member of the server, each marked online or offline.
    fn presence(&self) -> Vec<UserStatus> {
        let owner = self.db.owner().ok().flatten();
        let names = self.db.display_names().unwrap_or_default();
        self.db
            .member_names()
            .unwrap_or_default()
            .into_iter()
            .map(|id| {
                let online = self.online.contains(&id);
                let owner = owner.as_deref() == Some(id.as_str());
                let name = names.get(&id).cloned().unwrap_or_else(|| id.clone());
                UserStatus { id, name, online, owner }
            })
            .collect()
    }
}

type SharedState = Arc<Mutex<State>>;

/// Everything one server needs to start.
struct ServerConfig {
    addr: String,
    db_path: String,
    auth_addr: String,
    advertise: String,
    discovery_port: u16,
    exchange_secs: u64,
    peers: Vec<String>,
}

/// Usage: `server [ADDR] [DB_PATH] [AUTH_ADDR] [COUNT]`
///
/// With COUNT above 1, one process runs that many servers on consecutive ports, starting at
/// ADDR's port. Each gets its own database (DB_PATH with its port in the name) and its own
/// discovery port (the next one up from VAXELIUM_DISCOVERY_PORT).
fn main() -> anyhow::Result<()> {
    // Read .env before the runtime starts any threads.
    common::env::load();
    tokio::runtime::Runtime::new()?.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // `server forget-auth-pin [DB_PATH]`: after the auth service is reset on purpose, forget its
    // old certificate so this server accepts the new one. Run it while the server is stopped or
    // not; the next connection to the auth service remembers the new certificate.
    if argv.first().map(String::as_str) == Some("forget-auth-pin") {
        let db_path = argv.get(1).cloned().unwrap_or_else(|| DEFAULT_DB_PATH.to_string());
        let removed = Db::open(&db_path)?.forget_auth_pins()?;
        println!("forgot {removed} remembered auth service certificate(s) in {db_path}");
        return Ok(());
    }
    let mut args = argv.into_iter();
    let addr = args.next().unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let db_path = args.next().unwrap_or_else(|| DEFAULT_DB_PATH.to_string());
    let auth_addr = args.next().unwrap_or_else(|| DEFAULT_AUTH_ADDR.to_string());
    let count: u16 = match args.next() {
        Some(c) => c.parse().map_err(|_| anyhow::anyhow!("COUNT must be a number"))?,
        None => 1,
    };
    anyhow::ensure!(count >= 1, "COUNT must be at least 1");

    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("ADDR must look like host:port"))?;
    let port: u16 = port.parse()?;
    // Servers to start from, for finding others across the internet: VAXELIUM_PEERS=host:port,host:port
    let peers: Vec<String> = std::env::var("VAXELIUM_PEERS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(String::from)
        .collect();
    let discovery_base: u16 = env_number("VAXELIUM_DISCOVERY_PORT", DISCOVERY_PORT);
    let exchange_secs: u64 = env_number("VAXELIUM_EXCHANGE_SECS", 30);

    // Every server in this group, by address.
    let group: Vec<String> = (0..count)
        .map(|i| {
            port.checked_add(i)
                .map(|p| format!("{host}:{p}"))
                .ok_or_else(|| anyhow::anyhow!("too many servers for this port"))
        })
        .collect::<anyhow::Result<_>>()?;

    let mut tasks = Vec::new();
    for i in 0..count {
        let server_port = port
            .checked_add(i)
            .ok_or_else(|| anyhow::anyhow!("too many servers for this port"))?;
        let server_addr = group[i as usize].clone();
        let config = ServerConfig {
            advertise: if count == 1 {
                std::env::var("VAXELIUM_ADVERTISE").unwrap_or_else(|_| lan_address(&server_addr))
            } else {
                lan_address(&server_addr)
            },
            db_path: if count == 1 { db_path.clone() } else { numbered_path(&db_path, server_port) },
            addr: server_addr,
            auth_addr: auth_addr.clone(),
            discovery_port: discovery_base
                .checked_add(i)
                .ok_or_else(|| anyhow::anyhow!("too many servers for the discovery port"))?,
            exchange_secs,
            // The first server of a group starts from VAXELIUM_PEERS and knows every other server in
            // the group. The others find the rest of the group through it.
            peers: if i == 0 {
                peers.iter().cloned().chain(group.iter().skip(1).cloned()).collect()
            } else {
                Vec::new()
            },
        };
        tasks.push(tokio::spawn(run_server(config)));
    }
    for task in tasks {
        task.await??;
    }
    Ok(())
}

/// Runs one server: its database, its finding-others tasks, and its listener.
async fn run_server(config: ServerConfig) -> anyhow::Result<()> {
    let ServerConfig {
        addr,
        db_path,
        auth_addr,
        advertise,
        discovery_port,
        exchange_secs,
        peers,
    } = config;
    let auth_addr = Arc::new(auth_addr);

    let db = Db::open(&db_path)?;
    // This server's own certificate. It is made the first time the server runs.
    let (cert, key) = match (db.info_value("tls_cert")?, db.info_value("tls_key")?) {
        (Some(cert), Some(key)) => (cert, key),
        _ => {
            let identity = common::tls::new_identity()?;
            db.set_server_info("tls_cert", &identity.0)?;
            db.set_server_info("tls_key", &identity.1)?;
            identity
        }
    };
    let acceptor = common::tls::acceptor(&cert, &key)?;
    let state: SharedState = Arc::new(Mutex::new(State {
        db,
        online: Vec::new(),
        advertise: advertise.clone(),
        peer_failures: HashMap::new(),
    }));
    {
        let s = state.lock().unwrap();
        for peer in peers.iter().filter(|a| discovery::valid_addr(a) && a.as_str() != advertise) {
            let _ = s.db.add_peer(peer);
        }
    }

    // Finding other servers: LAN beacons, and asking known servers for their lists.
    tokio::spawn(discovery::beacons(state.clone(), discovery_port));
    tokio::spawn(discovery::exchange(state.clone(), Duration::from_secs(exchange_secs)));

    // Every connection subscribes to this and forwards events for its own channel.
    let (tx, _) = broadcast::channel::<ServerMsg>(256);

    let listener = TcpListener::bind(&addr).await?;
    println!("Vaxelium server listening on {addr} (TLS, known to others as {advertise}), storing data in {db_path}, accounts from {auth_addr}");

    loop {
        let (socket, peer) = listener.accept().await?;
        let state = state.clone();
        let tx = tx.clone();
        let auth_addr = auth_addr.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            println!("connection from {peer}");
            let socket = match acceptor.accept(socket).await {
                Ok(socket) => socket,
                Err(e) => {
                    eprintln!("{peer}: TLS handshake failed: {e}");
                    return;
                }
            };
            if let Err(e) = handle_connection(socket, state, tx, auth_addr).await {
                if !common::tls::is_quiet_close(&e) {
                    eprintln!("{peer}: {e}");
                }
            }
            println!("{peer} disconnected");
        });
    }
}

/// `vaxelium.db` with a port in its name: `vaxelium-7879.db`.
fn numbered_path(db_path: &str, port: u16) -> String {
    let path = std::path::Path::new(db_path);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "vaxelium".into());
    let name = match path.extension() {
        Some(ext) => format!("{stem}-{port}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{port}"),
    };
    path.with_file_name(name).to_string_lossy().into_owned()
}

async fn handle_connection<S>(
    socket: S,
    state: SharedState,
    tx: broadcast::Sender<ServerMsg>,
    auth_addr: Arc<String>,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(socket);
    let mut lines = BufReader::new(reader).lines();
    let mut rx = tx.subscribe();

    let Some(first) = lines.next_line().await? else {
        return Ok(());
    };
    // Public requests need no login: this server's details, or the servers it knows about.
    match serde_json::from_str::<ClientMsg>(&first) {
        Ok(ClientMsg::Info) => {
            send(&mut writer, &public_info(&state)).await?;
            return Ok(());
        }
        Ok(ClientMsg::Peers { known }) => {
            let addrs = share_peers(&state, known);
            send(&mut writer, &ServerMsg::Peers { addrs }).await?;
            return Ok(());
        }
        // Another server passing on a direct message from one of its members.
        Ok(ClientMsg::RelayDm { token, to, text }) => {
            let error = relay_receive(&state, &auth_addr, &tx, &token, &to, &text).await.err();
            send(&mut writer, &ServerMsg::RelayAck { error }).await?;
            return Ok(());
        }
        _ => {}
    }

    // Handshake: otherwise the first message must be a Connect carrying a session token.
    let session_token: String;
    let (user, display) = match serde_json::from_str::<ClientMsg>(&first) {
        Ok(ClientMsg::Connect { token }) => match check_session(&state, &auth_addr, &token).await {
            Ok(identity) => {
                session_token = token;
                (identity.id, identity.name)
            }
            Err(text) => {
                send(&mut writer, &ServerMsg::Error { text }).await?;
                return Ok(());
            }
        },
        _ => {
            send(&mut writer, &ServerMsg::Error { text: "connect with a session token first".into() }).await?;
            return Ok(());
        }
    };

    // The same account may be connected more than once (for example from two windows). Each
    // connection adds one entry to `online`, and the member shows as online while any is open.
    let (users, server_name, server_description, server_icon) = {
        let mut s = state.lock().unwrap();
        s.db.add_member(&user, &display, now_secs())?;
        s.online.push(user.clone());
        (s.presence(), s.db.server_name()?, s.db.server_description()?, s.db.server_icon()?)
    };

    let mut channel = "general".to_string();
    // Removes the user and broadcasts presence on every exit path, including errors.
    let _online = OnlineGuard { state: state.clone(), tx: tx.clone(), user: user.clone() };
    let server_public = state.lock().unwrap().db.is_public().unwrap_or(false);
    let welcome = ServerMsg::Welcome {
        name: display.clone(),
        user_id: user.clone(),
        server_name,
        server_description,
        server_icon,
        server_public,
    };
    send(&mut writer, &welcome).await?;
    {
        // Lets others message this account from another server. Does not hold up the welcome.
        let own = state.lock().unwrap().advertise.clone();
        let (state2, auth2, token2) = (state.clone(), auth_addr.to_string(), session_token.clone());
        tokio::spawn(async move {
            let _ = auth_call(&state2, &auth2, &AuthRequest::Seen { token: token2, addr: own }).await;
        });
    }
    send(&mut writer, &ServerMsg::DirectMessages { list: direct_message_list(&state, &user)? }).await?;
    let _ = tx.send(ServerMsg::Presence { users });
    let _ = tx.send(ServerMsg::System {
        channel: channel.clone(),
        text: format!("{user} connected"),
    });

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                let msg = match serde_json::from_str::<ClientMsg>(&line) {
                    Ok(m) => m,
                    Err(e) => {
                        send(&mut writer, &ServerMsg::Error { text: format!("bad message: {e}") }).await?;
                        continue;
                    }
                };

                match msg {
                    ClientMsg::RelayDm { .. } => {
                        send(&mut writer, &ServerMsg::Error { text: "that message is only accepted from another server".into() }).await?;
                    }
                    ClientMsg::Connect { .. } => {
                        send(&mut writer, &ServerMsg::Error { text: "already logged in".into() }).await?;
                    }

                    ClientMsg::Info => {
                        send(&mut writer, &public_info(&state)).await?;
                    }

                    ClientMsg::Logout => {
                        let _ = state.lock().unwrap().db.forget_cached_session(&session_token);
                        break;
                    }

                    ClientMsg::Peers { known } => {
                        let addrs = share_peers(&state, known);
                        send(&mut writer, &ServerMsg::Peers { addrs }).await?;
                    }

                    ClientMsg::OpenDm { name } => {
                        // A member of this server, or else an account found through the auth service.
                        let local = state.lock().unwrap().db.member_by_display_name(&name)?;
                        let other = match local {
                            Some(id) => Ok(id),
                            None => match auth_call(&state, &auth_addr, &AuthRequest::Lookup { name: name.clone() }).await {
                                Ok(AuthReply::Account { id, name: display }) => {
                                    state.lock().unwrap().db.add_person(&id, &display)?;
                                    Ok(id)
                                }
                                Ok(AuthReply::Error { text }) => Err(text),
                                _ => Err("could not look that person up".to_string()),
                            },
                        };
                        let opened = match other {
                            Err(text) => Err(text),
                            Ok(other) if other == user => Err("you cannot message yourself".to_string()),
                            Ok(other) => {
                                let is_local = state.lock().unwrap().db.is_member(&other)?;
                                if !is_local && locations_of(&state, &auth_addr, &other).await.is_empty() {
                                    Err(format!("{name} has not been online on any server yet"))
                                } else {
                                    let s = state.lock().unwrap();
                                    let joined = s.db.open_direct_message(&user, &other)?;
                                    let history = s.db.history(&joined)?;
                                    Ok((joined, history))
                                }
                            }
                        };
                        match opened {
                            Ok((joined, history)) => {
                                channel = joined.clone();
                                send(&mut writer, &ServerMsg::Joined { channel: joined, history }).await?;
                                send(&mut writer, &ServerMsg::DirectMessages { list: direct_message_list(&state, &user)? }).await?;
                            }
                            Err(text) => send(&mut writer, &ServerMsg::Error { text }).await?,
                        }
                    }

                    ClientMsg::FetchDm { channel } => {
                        let history = {
                            let s = state.lock().unwrap();
                            if s.db.is_direct_message_member(&channel, &user)? {
                                Some(s.db.history(&channel)?)
                            } else {
                                None
                            }
                        };
                        match history {
                            Some(messages) => send(&mut writer, &ServerMsg::DmHistory { channel, messages }).await?,
                            None => send(&mut writer, &ServerMsg::Error { text: "that direct message is not yours".into() }).await?,
                        }
                    }

                    ClientMsg::ListDirectMessages => {
                        send(&mut writer, &ServerMsg::DirectMessages { list: direct_message_list(&state, &user)? }).await?;
                    }

                    ClientMsg::ListChannels => {
                        let (channels, categories) = {
                            let s = state.lock().unwrap();
                            (s.db.channel_names()?, s.db.categories()?)
                        };
                        send(&mut writer, &ServerMsg::Channels { channels }).await?;
                        send(&mut writer, &ServerMsg::Categories { categories }).await?;
                    }

                    ClientMsg::CreateCategory { name } => {
                        let name = name.trim().to_string();
                        if name.is_empty() || name.chars().count() > MAX_CATEGORY_NAME_LEN {
                            send(&mut writer, &ServerMsg::Error {
                                text: format!("category name must be 1-{MAX_CATEGORY_NAME_LEN} characters"),
                            })
                            .await?;
                            continue;
                        }
                        let added = state.lock().unwrap().db.create_category(&name)?;
                        if added {
                            broadcast_categories(&state, &tx)?;
                        } else {
                            send(&mut writer, &ServerMsg::Error { text: "a category with that name already exists".into() }).await?;
                        }
                    }

                    ClientMsg::DeleteCategory { name } => {
                        let removed = state.lock().unwrap().db.delete_category(name.trim())?;
                        if removed {
                            broadcast_categories(&state, &tx)?;
                        } else {
                            send(&mut writer, &ServerMsg::Error { text: "there is no such category".into() }).await?;
                        }
                    }

                    ClientMsg::RenameChannel { channel, new_name } => {
                        let old = normalize_channel(&channel);
                        let new = normalize_channel(&new_name);
                        if old.is_empty() || old.starts_with("dm-") || new.is_empty() || new.starts_with("dm-") {
                            send(&mut writer, &ServerMsg::Error { text: "that channel cannot be renamed".into() }).await?;
                            continue;
                        }
                        let renamed = state.lock().unwrap().db.rename_channel(&old, &new)?;
                        if renamed {
                            let _ = tx.send(ServerMsg::ChannelRenamed { old, new });
                            broadcast_categories(&state, &tx)?;
                        } else {
                            send(&mut writer, &ServerMsg::Error {
                                text: "there is no such channel, or a channel with the new name already exists".into(),
                            })
                            .await?;
                        }
                    }

                    ClientMsg::MoveChannel { channel, category } => {
                        let channel = normalize_channel(&channel);
                        if channel.is_empty() || channel.starts_with("dm-") {
                            send(&mut writer, &ServerMsg::Error { text: "that channel cannot be moved".into() }).await?;
                            continue;
                        }
                        let category = category.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
                        let moved = {
                            let s = state.lock().unwrap();
                            match &category {
                                Some(c) if !s.db.category_exists(c)? => Err("there is no such category"),
                                _ => {
                                    s.db.move_channel(&channel, category.as_deref())?;
                                    Ok(())
                                }
                            }
                        };
                        match moved {
                            Ok(()) => broadcast_categories(&state, &tx)?,
                            Err(text) => send(&mut writer, &ServerMsg::Error { text: text.into() }).await?,
                        }
                    }

                    ClientMsg::SetServerIcon { png_base64 } => {
                        let png_base64 = png_base64.filter(|p| !p.is_empty());
                        if png_base64.as_ref().is_some_and(|p| p.len() > MAX_SERVER_ICON_LEN) {
                            send(&mut writer, &ServerMsg::Error {
                                text: "server icon is too large".into(),
                            })
                            .await?;
                            continue;
                        }
                        let result = {
                            let s = state.lock().unwrap();
                            if s.db.owner()?.as_deref() == Some(user.as_str()) {
                                s.db.set_server_icon(png_base64.as_deref())?;
                                Ok(())
                            } else {
                                Err("only the owner can change this server")
                            }
                        };
                        match result {
                            Ok(()) => {
                                let _ = tx.send(ServerMsg::ServerIcon { png_base64 });
                            }
                            Err(text) => send(&mut writer, &ServerMsg::Error { text: text.into() }).await?,
                        }
                    }

                    ClientMsg::UpdateServer { name, description } => {
                        let name = name.trim().to_string();
                        let description = description.trim().to_string();
                        if name.is_empty() || name.chars().count() > MAX_SERVER_NAME_LEN {
                            send(&mut writer, &ServerMsg::Error {
                                text: format!("server name must be 1-{MAX_SERVER_NAME_LEN} characters"),
                            })
                            .await?;
                            continue;
                        }
                        if description.chars().count() > MAX_SERVER_DESCRIPTION_LEN {
                            send(&mut writer, &ServerMsg::Error {
                                text: format!("description longer than {MAX_SERVER_DESCRIPTION_LEN} characters"),
                            })
                            .await?;
                            continue;
                        }
                        let result = {
                            let s = state.lock().unwrap();
                            if s.db.owner()?.as_deref() == Some(user.as_str()) {
                                s.db.set_server_name(&name)?;
                                s.db.set_server_description(&description)?;
                                Ok(())
                            } else {
                                Err("only the owner can change this server")
                            }
                        };
                        match result {
                            Ok(()) => {
                                let _ = tx.send(ServerMsg::ServerUpdated { name, description });
                            }
                            Err(text) => send(&mut writer, &ServerMsg::Error { text: text.into() }).await?,
                        }
                    }

                    ClientMsg::Join { channel: requested } => {
                        let requested = normalize_channel(&requested);
                        if requested.is_empty() {
                            send(&mut writer, &ServerMsg::Error { text: "invalid channel name".into() }).await?;
                            continue;
                        }
                        // Direct messages are joined only by their two members. Other names starting
                        // with "dm-" are refused, so a normal channel can never take one.
                        let history = {
                            let s = state.lock().unwrap();
                            if requested.starts_with("dm-") {
                                if s.db.is_direct_message_member(&requested, &user)? {
                                    Some(s.db.history(&requested)?)
                                } else {
                                    None
                                }
                            } else {
                                s.db.ensure_channel(&requested)?;
                                Some(s.db.history(&requested)?)
                            }
                        };
                        let Some(history) = history else {
                            send(&mut writer, &ServerMsg::Error { text: "that direct message is not yours".into() }).await?;
                            continue;
                        };
                        channel = requested.clone();
                        send(&mut writer, &ServerMsg::Joined { channel: requested, history }).await?;
                    }

                    ClientMsg::Say { text } => {
                        let text = text.trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        if text.chars().count() > MAX_TEXT_LEN {
                            send(&mut writer, &ServerMsg::Error {
                                text: format!("message longer than {MAX_TEXT_LEN} characters"),
                            })
                            .await?;
                            continue;
                        }
                        let mut message = ChatMessage {
                            id: 0,
                            channel: channel.clone(),
                            author: display.clone(),
                            author_id: user.clone(),
                            text,
                            timestamp: now_secs(),
                            edited: false,
                            reactions: Vec::new(),
                        };
                        message.id = state.lock().unwrap().db.save_message(&message)?;
                        let text = message.text.clone();
                        let _ = tx.send(ServerMsg::Message { message });
                        if let Some(other) = cross_server_other(&state, &channel, &user)? {
                            if let Err(e) = relay_out(&state, &auth_addr, &session_token, &other, &text).await {
                                send(&mut writer, &ServerMsg::Error { text: format!("saved here, but not delivered: {e}") }).await?;
                            }
                        }
                    }
                    ClientMsg::React { id, emoji } => {
                        if message_is_cross_server(&state, id)? {
                            send(&mut writer, &ServerMsg::Error { text: CROSS_SERVER_CHANGES.into() }).await?;
                            continue;
                        }
                        if !common::is_reaction(&emoji) {
                            send(&mut writer, &ServerMsg::Error { text: "that reaction is not allowed".into() }).await?;
                            continue;
                        }
                        let allowed = {
                            let s = state.lock().unwrap();
                            match s.db.message_channel(id)? {
                                Some(c) if c.starts_with("dm-") => s.db.is_direct_message_member(&c, &user)?,
                                _ => true,
                            }
                        };
                        if !allowed {
                            send(&mut writer, &ServerMsg::Error { text: "you cannot react to that message".into() }).await?;
                            continue;
                        }
                        let result = state.lock().unwrap().db.toggle_reaction(id, &emoji, &user)?;
                        match result {
                            Some((channel, reactions)) => {
                                let _ = tx.send(ServerMsg::Reactions { channel, id, reactions });
                            }
                            None => {
                                send(&mut writer, &ServerMsg::Error { text: "that message no longer exists".into() }).await?;
                            }
                        }
                    }
                    ClientMsg::Delete { id } => {
                        if message_is_cross_server(&state, id)? {
                            send(&mut writer, &ServerMsg::Error { text: CROSS_SERVER_CHANGES.into() }).await?;
                            continue;
                        }
                        // The author may delete a message, and so may the server owner.
                        let deleted = {
                            let s = state.lock().unwrap();
                            let is_owner = s.db.owner()?.as_deref() == Some(user.as_str());
                            s.db.delete_message(id, &user, is_owner)?
                        };
                        match deleted {
                            Some(channel) => {
                                let _ = tx.send(ServerMsg::MessageDeleted { channel, id });
                            }
                            None => {
                                send(&mut writer, &ServerMsg::Error {
                                    text: "only the author or the server owner can delete a message".into(),
                                })
                                .await?;
                            }
                        }
                    }
                    ClientMsg::Edit { id, text } => {
                        if message_is_cross_server(&state, id)? {
                            send(&mut writer, &ServerMsg::Error { text: CROSS_SERVER_CHANGES.into() }).await?;
                            continue;
                        }
                        let text = text.trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        if text.chars().count() > MAX_TEXT_LEN {
                            send(&mut writer, &ServerMsg::Error {
                                text: format!("message longer than {MAX_TEXT_LEN} characters"),
                            })
                            .await?;
                            continue;
                        }
                        let edited = state.lock().unwrap().db.edit_message(id, &user, &text)?;
                        match edited {
                            Some(channel) => {
                                let _ = tx.send(ServerMsg::MessageEdited { channel, id, text });
                            }
                            None => {
                                send(&mut writer, &ServerMsg::Error {
                                    text: "you can only edit your own messages".into(),
                                })
                                .await?;
                            }
                        }
                    }
                }
            }

            event = rx.recv() => {
                match event {
                    Ok(ServerMsg::Message { message }) => {
                        if message.channel == channel {
                            send(&mut writer, &ServerMsg::Message { message }).await?;
                        } else {
                            // Tell the client there is unread activity, without the content. A
                            // direct message is announced only to its members.
                            let allowed = {
                                let s = state.lock().unwrap();
                                !message.channel.starts_with("dm-")
                                    || s.db.is_direct_message_member(&message.channel, &user)?
                            };
                            if allowed {
                                send(&mut writer, &ServerMsg::Activity { channel: message.channel }).await?;
                            }
                        }
                    }
                    Ok(ServerMsg::System { channel: c, text }) if c == channel => {
                        send(&mut writer, &ServerMsg::System { channel: c, text }).await?;
                    }
                    Ok(ServerMsg::MessageEdited { channel: c, id, text }) if c == channel => {
                        send(&mut writer, &ServerMsg::MessageEdited { channel: c, id, text }).await?;
                    }
                    Ok(ServerMsg::MessageDeleted { channel: c, id }) if c == channel => {
                        send(&mut writer, &ServerMsg::MessageDeleted { channel: c, id }).await?;
                    }
                    Ok(ServerMsg::Reactions { channel: c, id, reactions }) if c == channel => {
                        send(&mut writer, &ServerMsg::Reactions { channel: c, id, reactions }).await?;
                    }
                    Ok(presence @ ServerMsg::Presence { .. }) => {
                        send(&mut writer, &presence).await?;
                    }
                    Ok(icon @ ServerMsg::ServerIcon { .. }) => {
                        send(&mut writer, &icon).await?;
                    }
                    Ok(updated @ ServerMsg::ServerUpdated { .. }) => {
                        send(&mut writer, &updated).await?;
                    }
                    Ok(cats @ ServerMsg::Categories { .. }) => {
                        send(&mut writer, &cats).await?;
                    }
                    Ok(ServerMsg::ChannelRenamed { old, new }) => {
                        // A connection that was in the channel follows it to its new name.
                        if channel == old {
                            channel = new.clone();
                        }
                        send(&mut writer, &ServerMsg::ChannelRenamed { old, new }).await?;
                    }
                    Ok(_) => {}
                    // Slow client fell behind; skip the dropped events and keep going.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    let _ = tx.send(ServerMsg::System {
        channel,
        text: format!("{user} disconnected"),
    });
    Ok(())
}

/// What a server tells anyone who asks, without logging in.
fn public_info(state: &SharedState) -> ServerMsg {
    let s = state.lock().unwrap();
    ServerMsg::Info {
        name: s.db.server_name().unwrap_or_default(),
        description: s.db.server_description().unwrap_or_default(),
        icon: s.db.server_icon().ok().flatten(),
        online: s.online.iter().collect::<std::collections::HashSet<_>>().len() as u32,
        public: s.db.is_public().unwrap_or(false),
    }
}

/// Takes in the servers a caller knows about, then answers with the servers this one knows,
/// including itself.
fn share_peers(state: &SharedState, known: Vec<String>) -> Vec<String> {
    let s = state.lock().unwrap();
    // Only other servers send a list of addresses; a client just asks.
    if !known.is_empty() {
        let _ = s.db.mark_public();
    }
    for addr in known {
        if addr != s.advertise && discovery::valid_addr(&addr) {
            let _ = s.db.add_peer(&addr);
        }
    }
    let mut addrs = vec![s.advertise.clone()];
    addrs.extend(
        s.db.peers()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| *a != s.advertise),
    );
    addrs
}

/// The address other servers should use to reach this one. A server bound to all interfaces
/// uses its LAN address; a server bound to one address uses that address.
fn lan_address(bind: &str) -> String {
    let Ok(sock) = bind.parse::<SocketAddr>() else {
        return bind.to_string();
    };
    if !sock.ip().is_unspecified() {
        return bind.to_string();
    }
    // Connecting a UDP socket sends nothing; it only picks the interface the LAN goes out on.
    let ip = StdUdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("10.255.255.255:1")?;
            s.local_addr()
        })
        .map(|a| a.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    format!("{ip}:{}", sock.port())
}

/// Reads a number from an environment variable, or uses the default.
fn env_number<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Asks the auth service who a session token belongs to. Returns the display name.
/// Trust on first use: remembers the certificate fingerprint of a server or the auth service the
/// first time it is seen, and refuses one that has changed since.
fn pin_ok(state: &SharedState, kind: &str, addr: &str, fingerprint: &str) -> bool {
    let key = format!("pin:{kind}:{addr}");
    let s = state.lock().unwrap();
    let remembered = s.db.info_value(&key).ok().flatten();
    match common::tls::check_pin(remembered.as_deref(), fingerprint) {
        Ok(true) => {
            let _ = s.db.set_server_info(&key, fingerprint);
            true
        }
        Ok(false) => true,
        Err(e) => {
            eprintln!("{addr}: {e}");
            false
        }
    }
}

/// The direct messages `user` is part of, each with the other member's display name.
fn direct_message_list(state: &SharedState, user: &str) -> rusqlite::Result<Vec<DirectMessage>> {
    let s = state.lock().unwrap();
    let names = s.db.display_names()?;
    let conversations = s.db.direct_messages_of(user)?;
    Ok(conversations
        .into_iter()
        .map(|(channel, other, last_message)| DirectMessage {
            name: names.get(&other).cloned().unwrap_or_else(|| other.clone()),
            channel,
            last_message,
            other_id: other,
        })
        .collect())
}

/// Works out who a connecting token belongs to. The auth service is asked first. If it cannot be
/// reached, a session it confirmed within `SESSION_CACHE_SECS` is still accepted. A session the
/// auth service rejects is refused at once and forgotten here.
/// Asks the auth service something and returns its reply. Its certificate is checked against the one
/// remembered, the same way `verify_session` does.
async fn auth_call(state: &SharedState, auth_addr: &str, request: &AuthRequest) -> Result<AuthReply, String> {
    let (mut stream, fingerprint) = common::tls::connect_async(auth_addr)
        .await
        .map_err(|e| format!("auth service unavailable: {e}"))?;
    if !pin_ok(state, "auth", auth_addr, &fingerprint) {
        return Err("the auth service's certificate has changed".into());
    }
    let mut line = serde_json::to_string(request).map_err(|e| e.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("auth service unavailable: {e}"))?;
    let mut lines = BufReader::new(stream).lines();
    let reply = lines
        .next_line()
        .await
        .map_err(|e| format!("auth service unavailable: {e}"))?
        .ok_or_else(|| "auth service closed the connection".to_string())?;
    serde_json::from_str(&reply).map_err(|e| format!("bad reply from auth service: {e}"))
}

/// The servers an account was last seen at, most recent first. Empty if none are known or the auth
/// service cannot be asked.
async fn locations_of(state: &SharedState, auth_addr: &str, id: &str) -> Vec<String> {
    match auth_call(state, auth_addr, &AuthRequest::Locate { id: id.to_string() }).await {
        Ok(AuthReply::Locations { addrs }) => addrs,
        _ => Vec::new(),
    }
}

/// Sends one message to another chat server and returns its answer. That server's certificate is
/// remembered the first time it is seen.
async fn server_call(state: &SharedState, addr: &str, msg: &ClientMsg) -> Result<ServerMsg, String> {
    let (mut stream, fingerprint) = common::tls::connect_async(addr)
        .await
        .map_err(|e| format!("could not reach {addr}: {e}"))?;
    if !pin_ok(state, "server", addr, &fingerprint) {
        return Err(format!("the certificate of {addr} has changed"));
    }
    let mut line = serde_json::to_string(msg).map_err(|e| e.to_string())?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut lines = BufReader::new(stream).lines();
    let reply = lines
        .next_line()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{addr} closed the connection"))?;
    serde_json::from_str(&reply).map_err(|e| e.to_string())
}

/// Stores a direct message that another server relayed from one of its members, and delivers it to
/// anyone here who is viewing it.
async fn relay_receive(
    state: &SharedState,
    auth_addr: &str,
    tx: &broadcast::Sender<ServerMsg>,
    token: &str,
    to: &str,
    text: &str,
) -> Result<(), String> {
    let sender = check_session(state, auth_addr, token).await?;
    if sender.id == to {
        return Err("you cannot message yourself".into());
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("the message is empty".into());
    }
    if text.chars().count() > MAX_TEXT_LEN {
        return Err(format!("message longer than {MAX_TEXT_LEN} characters"));
    }
    let message = {
        let s = state.lock().unwrap();
        s.db.add_person(&sender.id, &sender.name).map_err(|e| e.to_string())?;
        let channel = s.db.open_direct_message(&sender.id, to).map_err(|e| e.to_string())?;
        let mut message = ChatMessage {
            id: 0,
            channel,
            author: sender.name.clone(),
            author_id: sender.id.clone(),
            text,
            timestamp: now_secs(),
            edited: false,
            reactions: Vec::new(),
        };
        message.id = s.db.save_message(&message).map_err(|e| e.to_string())?;
        message
    };
    let _ = tx.send(ServerMsg::Message { message });
    Ok(())
}

/// Passes a direct message to the server the recipient was last seen at, so their copy is stored
/// there as well. Tries each known server in turn.
async fn relay_out(
    state: &SharedState,
    auth_addr: &str,
    token: &str,
    to: &str,
    text: &str,
) -> Result<(), String> {
    let own = state.lock().unwrap().advertise.clone();
    let mut last_error = "they have not been online on any server yet".to_string();
    for addr in locations_of(state, auth_addr, to).await.into_iter().filter(|a| *a != own) {
        let msg = ClientMsg::RelayDm { token: token.to_string(), to: to.to_string(), text: text.to_string() };
        match server_call(state, &addr, &msg).await {
            Ok(ServerMsg::RelayAck { error: None }) => return Ok(()),
            Ok(ServerMsg::RelayAck { error: Some(e) }) => last_error = e,
            Ok(_) => last_error = format!("{addr} gave an unexpected reply"),
            Err(e) => last_error = e,
        }
    }
    Err(last_error)
}

/// The other member of a cross-server direct message, when `channel` is one and `user` is in it.
fn cross_server_other(state: &SharedState, channel: &str, user: &str) -> rusqlite::Result<Option<String>> {
    let s = state.lock().unwrap();
    if !s.db.is_cross_server_dm(channel)? {
        return Ok(None);
    }
    Ok(s.db.direct_message_pair(channel)?.map(|(a, b)| if a == user { b } else { a }))
}

/// Whether a message is in a direct message kept in step with another server.
fn message_is_cross_server(state: &SharedState, id: i64) -> rusqlite::Result<bool> {
    let s = state.lock().unwrap();
    match s.db.message_channel(id)? {
        Some(channel) => s.db.is_cross_server_dm(&channel),
        None => Ok(false),
    }
}

/// Whether a direct message was relayed from or to another server, so it cannot be changed here.
const CROSS_SERVER_CHANGES: &str = "editing, deleting and reacting are not available on direct messages between servers yet";

async fn check_session(state: &SharedState, auth_addr: &str, token: &str) -> Result<Identity, String> {
    match verify_session(state, auth_addr, token).await {
        Ok(identity) => {
            let _ = state
                .lock()
                .unwrap()
                .db
                .cache_session(token, &identity.id, &identity.name, now_secs());
            Ok(identity)
        }
        Err(VerifyError::Rejected(text)) => {
            let _ = state.lock().unwrap().db.forget_cached_session(token);
            Err(text)
        }
        Err(VerifyError::Unreachable(text)) => {
            let cached = state.lock().unwrap().db.cached_session(token).ok().flatten();
            match cached {
                Some((id, name, verified)) if now_secs().saturating_sub(verified) <= SESSION_CACHE_SECS => {
                    println!("auth service unreachable, accepting {name} from the session cache");
                    Ok(Identity { id, name })
                }
                _ => Err(text),
            }
        }
    }
}

/// Who a session belongs to: the hidden account id, and the name to show.
struct Identity {
    id: String,
    name: String,
}

/// Why a session could not be confirmed by the auth service.
enum VerifyError {
    /// The auth service answered that the token is not valid, or its certificate changed.
    Rejected(String),
    /// The auth service could not be reached or did not answer properly.
    Unreachable(String),
}

/// Asks the auth service who a session token belongs to. Returns the display name.
async fn verify_session(state: &SharedState, auth_addr: &str, token: &str) -> Result<Identity, VerifyError> {
    use VerifyError::{Rejected, Unreachable};
    let (mut stream, fingerprint) = common::tls::connect_async(auth_addr)
        .await
        .map_err(|e| Unreachable(format!("auth service unavailable: {e}")))?;
    if !pin_ok(state, "auth", auth_addr, &fingerprint) {
        return Err(Rejected("the auth service's certificate has changed".into()));
    }
    let mut request = serde_json::to_string(&AuthRequest::Verify { token: token.to_string() })
        .map_err(|e| Unreachable(e.to_string()))?;
    request.push('\n');
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| Unreachable(format!("auth service unavailable: {e}")))?;

    let mut lines = BufReader::new(stream).lines();
    let line = lines
        .next_line()
        .await
        .map_err(|e| Unreachable(format!("auth service unavailable: {e}")))?
        .ok_or_else(|| Unreachable("auth service closed the connection".to_string()))?;
    match serde_json::from_str::<AuthReply>(&line) {
        Ok(AuthReply::Verified { id, name }) => Ok(Identity { id, name }),
        Ok(AuthReply::Error { text }) => Err(Rejected(text)),
        _ => Err(Unreachable("unexpected reply from auth service".into())),
    }
}

struct OnlineGuard {
    state: SharedState,
    tx: broadcast::Sender<ServerMsg>,
    user: String,
}

impl Drop for OnlineGuard {
    fn drop(&mut self) {
        let users = {
            let mut s = self.state.lock().unwrap();
            s.remove_online(&self.user);
            s.presence()
        };
        let _ = self.tx.send(ServerMsg::Presence { users });
    }
}

async fn send<W: AsyncWrite + Unpin>(writer: &mut W, msg: &ServerMsg) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Sends the current categories to every connection.
fn broadcast_categories(state: &Arc<Mutex<State>>, tx: &broadcast::Sender<ServerMsg>) -> Result<(), rusqlite::Error> {
    let categories = state.lock().unwrap().db.categories()?;
    let _ = tx.send(ServerMsg::Categories { categories });
    Ok(())
}

fn normalize_channel(name: &str) -> String {
    name.trim()
        .trim_start_matches('#')
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
