mod db;
mod discovery;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{
    ChannelRef,
    AuthReply, AuthRequest, ChatMessage, ClientMsg, DirectMessage, RoleInfo, ServerMsg, UserStatus, DEFAULT_ADDR,
    DEFAULT_AUTH_ADDR,
    DISCOVERY_PORT, EVERYONE_ROLE, MAX_ROLE_NAME_LEN, MAX_SERVER_DESCRIPTION_LEN, MAX_SERVER_ICON_LEN, perm,
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
    /// Peers removed for not answering, and when. Other servers keep listing them for a while, so
    /// they are not added back from those lists until the cooldown has passed.
    dead_peers: HashMap<String, std::time::Instant>,
}

impl State {
    /// True while a peer removed for not answering is still in its cooldown.
    fn recently_dead(&self, addr: &str) -> bool {
        self.dead_peers.get(addr).is_some_and(|at| at.elapsed() < discovery::DEAD_PEER_COOLDOWN)
    }
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
        let roles = self.db.all_member_roles().unwrap_or_default();
        let joined = self.db.first_seen_all().unwrap_or_default();
        self.db
            .member_names()
            .unwrap_or_default()
            .into_iter()
            .map(|id| {
                let online = self.online.contains(&id);
                let owner = owner.as_deref() == Some(id.as_str());
                let name = names.get(&id).cloned().unwrap_or_else(|| id.clone());
                let roles = roles.get(&id).cloned().unwrap_or_default();
                let joined = joined.get(&id).copied().unwrap_or(0);
                UserStatus { id, name, online, owner, roles, joined }
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
        dead_peers: HashMap::new(),
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

    // Members the auth service gave a random id since this server last ran: move what this server
    // stored under the old id to the new one. Needs the auth service; tried again on the next start.
    let legacy = state.lock().unwrap().db.legacy_ids()?;
    if !legacy.is_empty() {
        match auth_call(&state, &auth_addr, &AuthRequest::Migrations { old_ids: legacy }).await {
            Ok(AuthReply::Migrations { pairs }) => {
                let changed = state.lock().unwrap().db.apply_id_map(&pairs)?;
                println!("updated {} member id(s) to their new ids ({changed} references)", pairs.len());
            }
            Ok(_) => eprintln!("unexpected reply from the auth service about member ids"),
            Err(e) => eprintln!("could not update member ids yet ({e}); will try again on the next start"),
        }
    }

    let listener = TcpListener::bind(&addr).await?;
    println!("Vaxelium server listening on {addr} (TLS, known to others as {advertise}), storing data in {db_path}, accounts from {auth_addr}");

    loop {
        let (socket, peer) = listener.accept().await?;
        let state = state.clone();
        let tx = tx.clone();
        let auth_addr = auth_addr.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
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

    // No channel is open until the client joins one. Joining creates a channel only when it is asked for by name.
    let mut channel = String::new();
    let mut channel_id: i64 = 0;
    // Removes the user and broadcasts presence on every exit path, including errors.
    let _online = OnlineGuard { state: state.clone(), tx: tx.clone(), user: user.clone() };
    let server_public = state.lock().unwrap().db.is_public().unwrap_or(false);
    let server_id = state.lock().unwrap().db.info_value("server_id")?.unwrap_or_default();
    let welcome = ServerMsg::Welcome {
        server_id,
        name: display.clone(),
        user_id: user.clone(),
        server_name,
        server_description,
        server_icon,
        server_public,
    };
    send(&mut writer, &welcome).await?;
    let roles = state.lock().unwrap().db.role_list()?;
    send(&mut writer, &ServerMsg::Roles { roles }).await?;
    let overrides = state.lock().unwrap().db.channel_overrides()?;
    send(&mut writer, &ServerMsg::ChannelOverrides { overrides }).await?;
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
        channel_id,
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
                                    let id = s.db.ensure_channel(&joined)?;
                                    let history = s.db.history(id)?;
                                    Ok((joined, id, history))
                                }
                            }
                        };
                        match opened {
                            Ok((joined, id, history)) => {
                                channel = joined.clone();
                                channel_id = id;
                                send(&mut writer, &ServerMsg::Joined { channel: ChannelRef { id, name: joined }, history }).await?;
                                send(&mut writer, &ServerMsg::DirectMessages { list: direct_message_list(&state, &user)? }).await?;
                            }
                            Err(text) => send(&mut writer, &ServerMsg::Error { text }).await?,
                        }
                    }

                    ClientMsg::FetchDm { channel } => {
                        let history = {
                            let s = state.lock().unwrap();
                            if s.db.is_direct_message_member(&channel, &user)? {
                                let id = s.db.ensure_channel(&channel)?;
                                Some(s.db.history(id)?)
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
                        let (channels, categories, uncategorized) = {
                            let s = state.lock().unwrap();
                            let access = Access::new(&s.db, &user)?;
                            let (categories, uncategorized) = visible_layout(&access, s.db.categories()?, s.db.uncategorized_channels()?);
                            let channels = s.db.channel_list()?.into_iter().filter(|c| access.allows(c.id, perm::VIEW_CHANNEL)).collect();
                            (channels, categories, uncategorized)
                        };
                        send(&mut writer, &ServerMsg::Channels { channels }).await?;
                        send(&mut writer, &ServerMsg::Categories { categories, uncategorized }).await?;
                    }

                    ClientMsg::CreateCategory { name } => {
                        if !may(&state, &user, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let name = name.trim().to_string();
                        if name.is_empty() || name.chars().count() > MAX_CATEGORY_NAME_LEN {
                            send(&mut writer, &ServerMsg::Error {
                                text: format!("category name must be 1-{MAX_CATEGORY_NAME_LEN} characters"),
                            })
                            .await?;
                            continue;
                        }
                        let added = state.lock().unwrap().db.create_category(&name)?;
                        if added.is_some() {
                            broadcast_categories(&state, &tx)?;
                        } else {
                            send(&mut writer, &ServerMsg::Error { text: "a category with that name already exists".into() }).await?;
                        }
                    }

                    ClientMsg::DeleteCategory { category } => {
                        if !may(&state, &user, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let removed = state.lock().unwrap().db.delete_category(category)?;
                        if removed {
                            broadcast_categories(&state, &tx)?;
                        } else {
                            send(&mut writer, &ServerMsg::Error { text: "there is no such category".into() }).await?;
                        }
                    }

                    ClientMsg::RenameChannel { channel: id, new_name } => {
                        if !may(&state, &user, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let new = normalize_channel(&new_name);
                        let current = state.lock().unwrap().db.channel_name(id)?;
                        let renamable = matches!(&current, Some(name) if !name.starts_with("dm-"));
                        if new.is_empty() || new.starts_with("dm-") || !renamable {
                            send(&mut writer, &ServerMsg::Error { text: "that channel cannot be renamed".into() }).await?;
                            continue;
                        }
                        let renamed = state.lock().unwrap().db.rename_channel(id, &new)?;
                        if renamed {
                            let _ = tx.send(ServerMsg::ChannelRenamed { id, name: new });
                            broadcast_categories(&state, &tx)?;
                        } else {
                            send(&mut writer, &ServerMsg::Error {
                                text: "a channel with the new name already exists".into(),
                            })
                            .await?;
                        }
                    }

                    ClientMsg::DeleteChannel { channel: id } => {
                        if !may(&state, &user, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let current = state.lock().unwrap().db.channel_name(id)?;
                        let deletable = matches!(&current, Some(name) if !name.starts_with("dm-"));
                        if !deletable {
                            send(&mut writer, &ServerMsg::Error { text: "that channel cannot be deleted".into() }).await?;
                            continue;
                        }
                        state.lock().unwrap().db.delete_channel(id)?;
                        let _ = tx.send(ServerMsg::ChannelDeleted { id });
                        broadcast_categories(&state, &tx)?;
                    }

                    ClientMsg::MoveCategory { category, before } => {
                        if !may(&state, &user, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let moved = state.lock().unwrap().db.move_category(category, before)?;
                        if moved {
                            broadcast_categories(&state, &tx)?;
                        } else {
                            send(&mut writer, &ServerMsg::Error { text: "that category cannot be moved there".into() }).await?;
                        }
                    }

                    ClientMsg::PlaceChannel { channel: id, category, before } => {
                        if !may(&state, &user, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let current = state.lock().unwrap().db.channel_name(id)?;
                        let movable = matches!(&current, Some(name) if !name.starts_with("dm-"));
                        let category_ok = match category {
                            Some(c) => state.lock().unwrap().db.category_exists(c)?,
                            None => true,
                        };
                        if !movable || !category_ok {
                            send(&mut writer, &ServerMsg::Error { text: "that channel cannot be moved there".into() }).await?;
                            continue;
                        }
                        state.lock().unwrap().db.place_channel(id, category, before)?;
                        broadcast_categories(&state, &tx)?;
                    }

                    ClientMsg::MoveChannel { channel: id, category } => {
                        if !may(&state, &user, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let current = state.lock().unwrap().db.channel_name(id)?;
                        let movable = matches!(&current, Some(name) if !name.starts_with("dm-"));
                        if !movable {
                            send(&mut writer, &ServerMsg::Error { text: "that channel cannot be moved".into() }).await?;
                            continue;
                        }
                        let moved = {
                            let s = state.lock().unwrap();
                            match category {
                                Some(c) if !s.db.category_exists(c)? => Err("there is no such category"),
                                _ => {
                                    s.db.move_channel(id, category)?;
                                    Ok(())
                                }
                            }
                        };
                        match moved {
                            Ok(()) => broadcast_categories(&state, &tx)?,
                            Err(text) => send(&mut writer, &ServerMsg::Error { text: text.into() }).await?,
                        }
                    }

                    ClientMsg::CreateRole { name, color, permissions, mentionable, hoisted } => {
                        let change = RoleChange::Create { name, color, permissions, mentionable, hoisted };
                        if let Some(text) = change_roles(&state, &tx, &user, change)? {
                            send(&mut writer, &ServerMsg::Error { text }).await?;
                        }
                    }

                    ClientMsg::UpdateRole { role, name, color, permissions, mentionable, hoisted } => {
                        let change = RoleChange::Update { role, name, color, permissions, mentionable, hoisted };
                        if let Some(text) = change_roles(&state, &tx, &user, change)? {
                            send(&mut writer, &ServerMsg::Error { text }).await?;
                        }
                    }

                    ClientMsg::DeleteRole { role } => {
                        if let Some(text) = change_roles(&state, &tx, &user, RoleChange::Delete { role })? {
                            send(&mut writer, &ServerMsg::Error { text }).await?;
                        }
                    }

                    ClientMsg::MoveRole { role, before } => {
                        let change = RoleChange::Move { role, before };
                        if let Some(text) = change_roles(&state, &tx, &user, change)? {
                            send(&mut writer, &ServerMsg::Error { text }).await?;
                        }
                    }

                    ClientMsg::SetMemberRole { user: member, role, on } => {
                        let change = RoleChange::Member { user: member, role, on };
                        if let Some(text) = change_roles(&state, &tx, &user, change)? {
                            send(&mut writer, &ServerMsg::Error { text }).await?;
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
                        // Joining a channel that does not exist yet creates it, so that needs Manage channels.
                        let exists = state.lock().unwrap().db.channel_id(&requested)?.is_some();
                        if !exists && !requested.starts_with("dm-") && !may(&state, &user, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error {
                                text: "you need the Manage channels permission to create a channel".into(),
                            })
                            .await?;
                            continue;
                        }
                        let visible = {
                            let s = state.lock().unwrap();
                            match s.db.channel_id(&requested)? {
                                Some(id) => Access::new(&s.db, &user)?.allows(id, perm::VIEW_CHANNEL),
                                None => true,
                            }
                        };
                        if !visible {
                            send(&mut writer, &ServerMsg::Error { text: CANNOT_VIEW.into() }).await?;
                            continue;
                        }
                        match join_channel(&state, &user, &requested)? {
                            Some((id, history)) => {
                                channel = requested.clone();
                                channel_id = id;
                                send(&mut writer, &ServerMsg::Joined { channel: ChannelRef { id, name: requested }, history }).await?;
                            }
                            None => {
                                send(&mut writer, &ServerMsg::Error { text: "that direct message is not yours".into() }).await?;
                            }
                        }
                    }

                    ClientMsg::JoinChannel { id } => {
                        let name = state.lock().unwrap().db.channel_name(id)?;
                        let Some(name) = name else {
                            send(&mut writer, &ServerMsg::Error { text: "there is no such channel".into() }).await?;
                            continue;
                        };
                        let visible = {
                            let s = state.lock().unwrap();
                            Access::new(&s.db, &user)?.allows(id, perm::VIEW_CHANNEL)
                        };
                        if !visible {
                            send(&mut writer, &ServerMsg::Error { text: CANNOT_VIEW.into() }).await?;
                            continue;
                        }
                        match join_channel(&state, &user, &name)? {
                            Some((id, history)) => {
                                channel = name.clone();
                                channel_id = id;
                                send(&mut writer, &ServerMsg::Joined { channel: ChannelRef { id, name }, history }).await?;
                            }
                            None => {
                                send(&mut writer, &ServerMsg::Error { text: "that direct message is not yours".into() }).await?;
                            }
                        }
                    }

                    ClientMsg::Say { text } => {
                        if channel_id == 0 {
                            send(&mut writer, &ServerMsg::Error { text: "join a channel first".into() }).await?;
                            continue;
                        }
                        let access = Access::new(&state.lock().unwrap().db, &user)?;
                        if !access.allows(channel_id, perm::VIEW_CHANNEL) {
                            send(&mut writer, &ServerMsg::Error { text: CANNOT_VIEW.into() }).await?;
                            continue;
                        }
                        if !access.allows(channel_id, perm::SEND_MESSAGES) {
                            send(&mut writer, &ServerMsg::Error { text: CANNOT_SEND.into() }).await?;
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
                        let mut message = ChatMessage {
                            id: 0,
                            channel_id,
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
                    ClientMsg::SetChannelOverride { channel, role, view, send: send_setting } => {
                        if !may(&state, &user, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: "you need the Manage channels permission for that".into() }).await?;
                            continue;
                        }
                        let result = {
                            let s = state.lock().unwrap();
                            match s.db.channel_name(channel)? {
                                None => Err("there is no such channel".to_string()),
                                Some(name) if name.starts_with("dm-") => Err("direct messages can't have channel permissions".to_string()),
                                Some(_) if role != EVERYONE_ROLE && !s.db.role_list()?.iter().any(|r| r.id == role) => {
                                    Err("there is no such role".to_string())
                                }
                                Some(_) => {
                                    s.db.set_channel_override(channel, role, view, send_setting)?;
                                    Ok(s.db.channel_overrides()?)
                                }
                            }
                        };
                        match result {
                            Ok(overrides) => {
                                let _ = tx.send(ServerMsg::ChannelOverrides { overrides });
                            }
                            Err(text) => send(&mut writer, &ServerMsg::Error { text }).await?,
                        }
                    }

                    ClientMsg::React { id, emoji } => {
                        if !message_visible(&state, &user, id)? {
                            send(&mut writer, &ServerMsg::Error { text: CANNOT_VIEW.into() }).await?;
                            continue;
                        }
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
                            Some((channel_id, reactions)) => {
                                let _ = tx.send(ServerMsg::Reactions { channel_id, id, reactions });
                            }
                            None => {
                                send(&mut writer, &ServerMsg::Error { text: "that message no longer exists".into() }).await?;
                            }
                        }
                    }
                    ClientMsg::Delete { id } => {
                        if !message_visible(&state, &user, id)? {
                            send(&mut writer, &ServerMsg::Error { text: CANNOT_VIEW.into() }).await?;
                            continue;
                        }
                        if message_is_cross_server(&state, id)? {
                            send(&mut writer, &ServerMsg::Error { text: CROSS_SERVER_CHANGES.into() }).await?;
                            continue;
                        }
                        // The author may delete a message, and so may anyone with Manage messages.
                        let any = may(&state, &user, perm::MANAGE_MESSAGES)?;
                        let deleted = state.lock().unwrap().db.delete_message(id, &user, any)?;
                        match deleted {
                            Some(channel_id) => {
                                let _ = tx.send(ServerMsg::MessageDeleted { channel_id, id });
                            }
                            None => {
                                send(&mut writer, &ServerMsg::Error {
                                    text: "only the author, or someone with Manage messages, can delete a message".into(),
                                })
                                .await?;
                            }
                        }
                    }
                    ClientMsg::Edit { id, text } => {
                        if !message_visible(&state, &user, id)? {
                            send(&mut writer, &ServerMsg::Error { text: CANNOT_VIEW.into() }).await?;
                            continue;
                        }
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
                            Some(channel_id) => {
                                let _ = tx.send(ServerMsg::MessageEdited { channel_id, id, text });
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
                        if message.channel_id == channel_id {
                            let viewable = Access::new(&state.lock().unwrap().db, &user)?.allows(channel_id, perm::VIEW_CHANNEL);
                            if viewable {
                                send(&mut writer, &ServerMsg::Message { message }).await?;
                            }
                        } else {
                            // Tell the client there is unread activity, without the content. A
                            // direct message is announced only to its members, and a channel only
                            // to members who may view it.
                            let allowed = {
                                let s = state.lock().unwrap();
                                let access = Access::new(&s.db, &user)?;
                                access.allows(message.channel_id, perm::VIEW_CHANNEL)
                                    && (!message.channel.starts_with("dm-")
                                        || s.db.is_direct_message_member(&message.channel, &user)?)
                            };
                            if allowed {
                                send(&mut writer, &ServerMsg::Activity { channel_id: message.channel_id }).await?;
                            }
                        }
                    }
                    Ok(ServerMsg::System { channel_id: c, text }) if c == channel_id => {
                        if Access::new(&state.lock().unwrap().db, &user)?.allows(c, perm::VIEW_CHANNEL) {
                            send(&mut writer, &ServerMsg::System { channel_id: c, text }).await?;
                        }
                    }
                    Ok(ServerMsg::MessageEdited { channel_id: c, id, text }) if c == channel_id => {
                        if Access::new(&state.lock().unwrap().db, &user)?.allows(c, perm::VIEW_CHANNEL) {
                            send(&mut writer, &ServerMsg::MessageEdited { channel_id: c, id, text }).await?;
                        }
                    }
                    Ok(ServerMsg::MessageDeleted { channel_id: c, id }) if c == channel_id => {
                        if Access::new(&state.lock().unwrap().db, &user)?.allows(c, perm::VIEW_CHANNEL) {
                            send(&mut writer, &ServerMsg::MessageDeleted { channel_id: c, id }).await?;
                        }
                    }
                    Ok(ServerMsg::Reactions { channel_id: c, id, reactions }) if c == channel_id => {
                        if Access::new(&state.lock().unwrap().db, &user)?.allows(c, perm::VIEW_CHANNEL) {
                            send(&mut writer, &ServerMsg::Reactions { channel_id: c, id, reactions }).await?;
                        }
                    }
                    Ok(overrides @ ServerMsg::ChannelOverrides { .. }) => {
                        send(&mut writer, &overrides).await?;
                    }
                    Ok(presence @ ServerMsg::Presence { .. }) => {
                        send(&mut writer, &presence).await?;
                    }
                    Ok(roles @ ServerMsg::Roles { .. }) => {
                        send(&mut writer, &roles).await?;
                    }
                    Ok(icon @ ServerMsg::ServerIcon { .. }) => {
                        send(&mut writer, &icon).await?;
                    }
                    Ok(updated @ ServerMsg::ServerUpdated { .. }) => {
                        send(&mut writer, &updated).await?;
                    }
                    Ok(ServerMsg::Categories { .. }) => {
                        // Each member gets the layout with only the channels they may view.
                        let (categories, uncategorized) = {
                            let s = state.lock().unwrap();
                            let access = Access::new(&s.db, &user)?;
                            visible_layout(&access, s.db.categories()?, s.db.uncategorized_channels()?)
                        };
                        send(&mut writer, &ServerMsg::Categories { categories, uncategorized }).await?;
                    }
                    Ok(ServerMsg::ChannelDeleted { id }) => {
                        // A connection that had the channel open moves to general.
                        if channel_id == id {
                            channel = String::new();
                            channel_id = 0;
                        }
                        send(&mut writer, &ServerMsg::ChannelDeleted { id }).await?;
                    }
                    Ok(ServerMsg::ChannelRenamed { id, name }) => {
                        // A connection that was in the channel follows it to its new name.
                        if channel_id == id {
                            channel = name.clone();
                        }
                        send(&mut writer, &ServerMsg::ChannelRenamed { id, name }).await?;
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
        channel_id,
        text: format!("{user} disconnected"),
    });
    Ok(())
}

/// What a server tells anyone who asks, without logging in.
fn public_info(state: &SharedState) -> ServerMsg {
    let s = state.lock().unwrap();
    ServerMsg::Info {
        server_id: s.db.info_value("server_id").ok().flatten().unwrap_or_default(),
        name: s.db.server_name().unwrap_or_default(),
        description: s.db.server_description().unwrap_or_default(),
        icon: s.db.server_icon().ok().flatten(),
        online: s.online.iter().collect::<std::collections::HashSet<_>>().len() as u32,
        public: s.db.is_public().unwrap_or(false),
        // Every server makes its own certificate on first run (see `common::tls`).
        self_signed: true,
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
        if addr != s.advertise && discovery::valid_addr(&addr) && !s.recently_dead(&addr) {
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
            channel_id: s.db.ensure_channel(&channel).unwrap_or_default(),
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
        let channel_id = s.db.ensure_channel(&channel).map_err(|e| e.to_string())?;
        let mut message = ChatMessage {
            id: 0,
            channel_id,
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

/// Joins a channel by its name for this account. Returns the channel's id and its history, or `None`
/// when the channel is a direct message the account is not a member of.
fn join_channel(state: &SharedState, user: &str, name: &str) -> Result<Option<(i64, Vec<ChatMessage>)>, rusqlite::Error> {
    // Direct messages are joined only by their two members. Other names starting with "dm-" are
    // refused, so a normal channel can never take one.
    let s = state.lock().unwrap();
    let id = if name.starts_with("dm-") {
        if !s.db.is_direct_message_member(name, user)? {
            return Ok(None);
        }
        s.db.ensure_channel(name)?
    } else {
        s.db.ensure_channel(name)?
    };
    let history = s.db.history(id)?;
    Ok(Some((id, history)))
}

const NO_CHANNEL_PERMISSION: &str = "you need the Manage channels permission for that";

/// What an account may do, worked out from its roles.
struct Power {
    owner: bool,
    /// The permissions its roles grant together.
    perms: u32,
    /// Where its highest role sits in the role order (highest first), if it has any roles.
    top: Option<usize>,
}

impl Power {
    fn has(&self, permission: u32) -> bool {
        self.owner || self.perms & perm::ADMINISTRATOR != 0 || self.perms & permission == permission
    }

    /// The permissions this member may give to a role: all of them for an owner or administrator.
    fn grantable(&self) -> u32 {
        if self.owner || self.perms & perm::ADMINISTRATOR != 0 {
            perm::ALL
        } else {
            self.perms
        }
    }

    /// True when the account is above the role at `index` in the role order, so it may manage it.
    fn outranks(&self, index: usize) -> bool {
        self.owner || self.top.is_some_and(|top| index > top)
    }
}

/// Works out what `user` may do. `order` is every role, highest first.
fn power_of(db: &Db, user: &str, order: &[RoleInfo]) -> rusqlite::Result<Power> {
    if db.owner()?.as_deref() == Some(user) {
        return Ok(Power { owner: true, perms: perm::ALL, top: None });
    }
    let mine = db.member_role_ids(user)?;
    // Everyone has @everyone, so its permissions apply first. It never counts as a member's highest role.
    let mut perms = order.iter().find(|r| r.id == EVERYONE_ROLE).map_or(0, |r| r.permissions);
    let mut top = None;
    for (index, role) in order.iter().enumerate() {
        if role.id != EVERYONE_ROLE && mine.contains(&role.id) {
            perms |= role.permissions;
            top.get_or_insert(index);
        }
    }
    Ok(Power { owner: false, perms, top })
}

/// True when `user` has the permission. The owner has every permission.
fn may(state: &SharedState, user: &str, permission: u32) -> rusqlite::Result<bool> {
    let s = state.lock().unwrap();
    let order = s.db.role_list()?;
    Ok(power_of(&s.db, user, &order)?.has(permission))
}

/// What one member may see and say. Worked out once from their roles, the owner, and the channel
/// exceptions. A channel's exception for a role wins over the role's server-wide permission, and a
/// denial from any of the member's roles wins over an allowance.
struct Access {
    power: Power,
    /// The roles the member holds, @everyone included.
    held: Vec<i64>,
    overrides: Vec<common::ChannelOverride>,
    /// The name of each channel, by id. Direct messages are told apart by their name.
    names: std::collections::HashMap<i64, String>,
}

impl Access {
    fn new(db: &Db, user: &str) -> rusqlite::Result<Self> {
        let order = db.role_list()?;
        let power = power_of(db, user, &order)?;
        let mut held = db.member_role_ids(user)?;
        held.push(EVERYONE_ROLE);
        Ok(Access { power, held, overrides: db.channel_overrides()?, names: db.channel_names()? })
    }

    /// Whether the member may view (`VIEW_CHANNEL`) or send in (`SEND_MESSAGES`) a channel. Direct
    /// messages are allowed here, since their members are checked where they are used.
    fn allows(&self, channel_id: i64, bit: u32) -> bool {
        match self.names.get(&channel_id) {
            None => return false,
            Some(name) if name.starts_with("dm-") => return true,
            Some(_) => {}
        }
        if self.power.has(perm::MANAGE_CHANNELS) {
            return true;
        }
        let mut allowed = false;
        for o in self.overrides.iter().filter(|o| o.channel_id == channel_id && self.held.contains(&o.role)) {
            let setting = if bit == perm::VIEW_CHANNEL { o.view } else { o.send };
            match setting {
                Some(false) => return false,
                Some(true) => allowed = true,
                None => {}
            }
        }
        allowed || self.power.has(bit)
    }
}

/// The categories and the channels outside them, with only the channels this member may view.
fn visible_layout(access: &Access, categories: Vec<common::CategoryInfo>, uncategorized: Vec<common::ChannelRef>) -> (Vec<common::CategoryInfo>, Vec<common::ChannelRef>) {
    let categories = categories
        .into_iter()
        .map(|mut c| {
            c.channels.retain(|ch| access.allows(ch.id, perm::VIEW_CHANNEL));
            c
        })
        .collect();
    let uncategorized = uncategorized.into_iter().filter(|ch| access.allows(ch.id, perm::VIEW_CHANNEL)).collect();
    (categories, uncategorized)
}

/// Whether `user` may view the channel a message is in. False when the message does not exist.
fn message_visible(state: &SharedState, user: &str, id: i64) -> rusqlite::Result<bool> {
    let s = state.lock().unwrap();
    let Some(name) = s.db.message_channel(id)? else {
        return Ok(false);
    };
    let Some(channel_id) = s.db.channel_id(&name)? else {
        return Ok(false);
    };
    Ok(Access::new(&s.db, user)?.allows(channel_id, perm::VIEW_CHANNEL))
}

/// Says the channel cannot be used by this member, for the message that refuses them.
const CANNOT_VIEW: &str = "you can't view that channel";
const CANNOT_SEND: &str = "you can't send messages in that channel";

/// The permissions @everyone keeps until another role has them.
const EVERYONE_KEEPS_BITS: u32 = perm::VIEW_CHANNEL | perm::SEND_MESSAGES;
const EVERYONE_KEEPS: &str = "@everyone keeps View channels and Message channels until another role has them";

/// True when setting @everyone's permissions to `permissions` would drop one of the kept bits that no
/// other role holds.
fn removes_everyone_keep(db: &Db, permissions: u32) -> rusqlite::Result<bool> {
    let others = db.role_list()?.into_iter().filter(|r| r.id != EVERYONE_ROLE).map(|r| r.permissions).collect::<Vec<_>>();
    for bit in [perm::VIEW_CHANNEL, perm::SEND_MESSAGES] {
        if EVERYONE_KEEPS_BITS & bit != 0 && permissions & bit == 0 && !others.iter().any(|p| p & (bit | perm::ADMINISTRATOR) != 0) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A change to the roles, asked for by a member.
enum RoleChange {
    Create { name: String, color: String, permissions: u32, mentionable: bool, hoisted: bool },
    Update { role: i64, name: String, color: String, permissions: u32, mentionable: bool, hoisted: bool },
    Delete { role: i64 },
    Move { role: i64, before: Option<i64> },
    Member { user: String, role: i64, on: bool },
}

/// Why a role's name, colour or permissions are not allowed, or `None` when they are fine.
fn role_fields_error(power: &Power, name: &str, color: &str, permissions: u32) -> Option<String> {
    if name.is_empty() || name.chars().count() > MAX_ROLE_NAME_LEN {
        return Some(format!("role name must be 1-{MAX_ROLE_NAME_LEN} characters"));
    }
    let color_ok = color.is_empty()
        || (color.len() == 7 && color.starts_with('#') && color[1..].bytes().all(|b| b.is_ascii_hexdigit()));
    if !color_ok {
        return Some("colour must look like #rrggbb".into());
    }
    if permissions & !perm::ALL != 0 {
        return Some("that permission does not exist".into());
    }
    if permissions & !power.grantable() != 0 {
        return Some("you cannot give a role permissions you do not have".into());
    }
    None
}

/// Applies a role change for `actor`. `Ok(Err(text))` means it was refused, and why.
fn apply_role_change(db: &Db, actor: &str, change: RoleChange) -> rusqlite::Result<Result<(), String>> {
    let order = db.role_list()?;
    let power = power_of(db, actor, &order)?;
    if !power.has(perm::MANAGE_ROLES) {
        return Ok(Err("you need the Manage roles permission for that".into()));
    }
    let index_of = |id: i64| order.iter().position(|r| r.id == id);
    let below_you = "that role is not below your highest role";
    let no_role = "there is no such role";
    match change {
        RoleChange::Create { name, color, permissions, mentionable, hoisted } => {
            let name = name.trim().to_string();
            if let Some(text) = role_fields_error(&power, &name, &color, permissions) {
                return Ok(Err(text));
            }
            match db.create_role(&name, &color, permissions, mentionable, hoisted)? {
                Some(_) => Ok(Ok(())),
                None => Ok(Err("a role with that name already exists".into())),
            }
        }
        RoleChange::Update { role, name, color, permissions, mentionable, hoisted } => {
            // @everyone keeps its name.
            let name = if role == EVERYONE_ROLE { "@everyone".to_string() } else { name.trim().to_string() };
            // @everyone already reaches everyone, so it cannot be pinged or grouped.
            let (mentionable, hoisted) = if role == EVERYONE_ROLE { (false, false) } else { (mentionable, hoisted) };
            let Some(index) = index_of(role) else { return Ok(Err(no_role.into())) };
            if !power.outranks(index) {
                return Ok(Err(below_you.into()));
            }
            if let Some(text) = role_fields_error(&power, &name, &color, permissions) {
                return Ok(Err(text));
            }
            if role == EVERYONE_ROLE && permissions & perm::ADMINISTRATOR != 0 {
                return Ok(Err("@everyone cannot be an administrator".into()));
            }
            if role == EVERYONE_ROLE && removes_everyone_keep(&db, permissions)? {
                return Ok(Err(EVERYONE_KEEPS.into()));
            }
            if db.role_name_taken(&name, role)? {
                return Ok(Err("a role with that name already exists".into()));
            }
            db.update_role(role, &name, &color, permissions, mentionable, hoisted)?;
            Ok(Ok(()))
        }
        RoleChange::Delete { role } => {
            if role == EVERYONE_ROLE {
                return Ok(Err("@everyone cannot be deleted".into()));
            }
            let Some(index) = index_of(role) else { return Ok(Err(no_role.into())) };
            if !power.outranks(index) {
                return Ok(Err(below_you.into()));
            }
            db.delete_role(role)?;
            Ok(Ok(()))
        }
        RoleChange::Move { role, before } => {
            if role == EVERYONE_ROLE {
                return Ok(Err("@everyone stays at the bottom".into()));
            }
            let Some(index) = index_of(role) else { return Ok(Err(no_role.into())) };
            if !power.outranks(index) {
                return Ok(Err(below_you.into()));
            }
            // Only the real roles are reordered; @everyone stays at the bottom.
            let mut ids: Vec<i64> = order.iter().map(|r| r.id).filter(|&id| id != EVERYONE_ROLE).collect();
            ids.retain(|&id| id != role);
            let at = match before {
                Some(b) => match ids.iter().position(|&id| id == b) {
                    Some(at) => at,
                    None => return Ok(Err(no_role.into())),
                },
                None => ids.len(),
            };
            ids.insert(at, role);
            // The role must still sit below the sender's highest role after the move.
            if !power.owner {
                let top_id = power.top.map(|t| order[t].id);
                let moved_at = ids.iter().position(|&id| id == role);
                let top_at = top_id.and_then(|t| ids.iter().position(|&id| id == t));
                if !matches!((moved_at, top_at), (Some(m), Some(t)) if m > t) {
                    return Ok(Err(below_you.into()));
                }
            }
            db.set_role_order(&ids)?;
            Ok(Ok(()))
        }
        RoleChange::Member { user, role, on } => {
            if role == EVERYONE_ROLE {
                return Ok(Err("@everyone is given to everyone automatically".into()));
            }
            let Some(index) = index_of(role) else { return Ok(Err(no_role.into())) };
            if !power.outranks(index) {
                return Ok(Err(below_you.into()));
            }
            if !db.is_member(&user)? {
                return Ok(Err("that person is not a member of this server".into()));
            }
            let target = power_of(db, &user, &order)?;
            // The owner outranks every role, so only the owner may change the owner's roles.
            if target.owner && !power.owner {
                return Ok(Err("only the owner can change the owner's roles".into()));
            }
            if !target.top.map_or(true, |top| power.outranks(top)) {
                return Ok(Err("that member's highest role is not below yours".into()));
            }
            db.set_member_role(&user, role, on)?;
            Ok(Ok(()))
        }
    }
}

/// Applies a role change and tells everyone about the new roles and members. Returns why the change
/// was refused, if it was.
fn change_roles(
    state: &SharedState,
    tx: &broadcast::Sender<ServerMsg>,
    user: &str,
    change: RoleChange,
) -> rusqlite::Result<Option<String>> {
    let outcome = apply_role_change(&state.lock().unwrap().db, user, change)?;
    match outcome {
        Ok(()) => {
            let (roles, users) = {
                let s = state.lock().unwrap();
                (s.db.role_list()?, s.presence())
            };
            let _ = tx.send(ServerMsg::Roles { roles });
            let _ = tx.send(ServerMsg::Presence { users });
            Ok(None)
        }
        Err(text) => Ok(Some(text)),
    }
}

/// Sends the current categories to every connection.
fn broadcast_categories(state: &Arc<Mutex<State>>, tx: &broadcast::Sender<ServerMsg>) -> Result<(), rusqlite::Error> {
    let (categories, uncategorized) = {
        let s = state.lock().unwrap();
        (s.db.categories()?, s.db.uncategorized_channels()?)
    };
    let _ = tx.send(ServerMsg::Categories { categories, uncategorized });
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

#[cfg(test)]
mod role_tests {
    use super::*;

    /// The owner, then "bob" who has the Admin role (manage roles and channels) and "carol" who has none.
    fn server() -> (Db, i64, i64) {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("bob", "Bob", 2).unwrap();
        db.add_member("carol", "Carol", 3).unwrap();
        db.add_member("dan", "Dan", 4).unwrap();
        let admin = db
            .create_role("Admin", "", perm::MANAGE_ROLES | perm::MANAGE_CHANNELS, false, false)
            .unwrap()
            .unwrap();
        let mods = db.create_role("Mods", "#00ff00", perm::MANAGE_MESSAGES, false, false).unwrap().unwrap();
        // Admin sits above Mods, as it does after creation order: Mods was created last, so is lowest.
        db.set_role_order(&[admin, mods]).unwrap();
        db.set_member_role("bob", admin, true).unwrap();
        db.set_member_role("dan", admin, true).unwrap();
        (db, admin, mods)
    }

    #[test]
    fn channel_view_and_send_follow_roles_and_channel_exceptions() {
        let (db, _, _) = server();
        db.add_member("erin", "Erin", 5).unwrap();
        let general = db.ensure_channel("general").unwrap();
        let readers = db.create_role("Readers", "", perm::VIEW_CHANNEL | perm::SEND_MESSAGES, false, false).unwrap().unwrap();
        let viewers = db.create_role("Viewers", "", perm::VIEW_CHANNEL, false, false).unwrap().unwrap();
        db.set_member_role("bob", readers, true).unwrap();
        db.set_member_role("carol", viewers, true).unwrap();
        // Take the channel permissions off @everyone. Other roles hold them, so that is allowed.
        let off = apply_role_change(
            &db,
            "owner",
            RoleChange::Update { role: EVERYONE_ROLE, name: String::new(), color: String::new(), permissions: 0, mentionable: false, hoisted: false },
        )
        .unwrap();
        assert_eq!(off, Ok(()));

        // Nobody sees a channel without a role that gives it. Owners and managers see every channel.
        assert!(!Access::new(&db, "erin").unwrap().allows(general, perm::VIEW_CHANNEL));
        assert!(Access::new(&db, "owner").unwrap().allows(general, perm::SEND_MESSAGES));

        let bob = Access::new(&db, "bob").unwrap();
        assert!(bob.allows(general, perm::VIEW_CHANNEL) && bob.allows(general, perm::SEND_MESSAGES));

        // A viewer may read, but not send.
        let carol = Access::new(&db, "carol").unwrap();
        assert!(carol.allows(general, perm::VIEW_CHANNEL));
        assert!(!carol.allows(general, perm::SEND_MESSAGES));

        // A denial on the channel for one of the member's roles hides it, even with a server-wide grant.
        db.set_channel_override(general, viewers, Some(false), None).unwrap();
        assert!(!Access::new(&db, "carol").unwrap().allows(general, perm::VIEW_CHANNEL));

        // An allowance on the channel gives a role access it does not have server-wide.
        db.set_channel_override(general, viewers, None, None).unwrap();
        db.set_channel_override(general, EVERYONE_ROLE, Some(true), None).unwrap();
        assert!(Access::new(&db, "erin").unwrap().allows(general, perm::VIEW_CHANNEL));

        // A send denial stops sending but not viewing.
        db.set_channel_override(general, viewers, None, Some(false)).unwrap();
        assert!(!Access::new(&db, "carol").unwrap().allows(general, perm::SEND_MESSAGES));
        assert!(Access::new(&db, "carol").unwrap().allows(general, perm::VIEW_CHANNEL));

        // Removing an exception puts the server-wide permission back.
        db.set_channel_override(general, viewers, None, None).unwrap();
        assert!(!Access::new(&db, "carol").unwrap().allows(general, perm::SEND_MESSAGES));
        assert!(db.channel_overrides().unwrap().iter().all(|o| o.role != viewers));
    }

    #[test]
    fn ping_and_member_list_flags_are_saved_and_changed() {
        let (db, _, _) = server();
        let made = apply_role_change(
            &db,
            "owner",
            RoleChange::Create { name: "Pingers".into(), color: String::new(), permissions: 0, mentionable: true, hoisted: true },
        )
        .unwrap();
        assert!(made.is_ok());
        let pingers = db.role_list().unwrap().into_iter().find(|r| r.name == "Pingers").unwrap();
        assert!(pingers.mentionable && pingers.hoisted);
        let off = apply_role_change(
            &db,
            "owner",
            RoleChange::Update { role: pingers.id, name: "Pingers".into(), color: String::new(), permissions: 0, mentionable: false, hoisted: false },
        )
        .unwrap();
        assert!(off.is_ok());
        let pingers = db.role_list().unwrap().into_iter().find(|r| r.id == pingers.id).unwrap();
        assert!(!pingers.mentionable && !pingers.hoisted);
    }

    #[test]
    fn owner_can_do_everything() {
        let (db, _, _) = server();
        let made = apply_role_change(
            &db,
            "owner",
            RoleChange::Create { name: "Helpers".into(), color: "#123456".into(), permissions: perm::ALL , mentionable: false, hoisted: false },
        )
        .unwrap();
        assert_eq!(made, Ok(()));
        assert!(db.role_list().unwrap().iter().any(|r| r.name == "Helpers"));
    }

    #[test]
    fn members_without_manage_roles_cannot_change_roles() {
        let (db, _, mods) = server();
        let refused = apply_role_change(
            &db,
            "carol",
            RoleChange::Member { user: "carol".into(), role: mods, on: true },
        )
        .unwrap();
        assert!(refused.is_err());
        let refused = apply_role_change(
            &db,
            "carol",
            RoleChange::Create { name: "X".into(), color: String::new(), permissions: 0 , mentionable: false, hoisted: false },
        )
        .unwrap();
        assert!(refused.is_err());
    }

    #[test]
    fn admins_give_roles_below_them_only() {
        let (db, admin, mods) = server();
        let given = apply_role_change(
            &db,
            "bob",
            RoleChange::Member { user: "carol".into(), role: mods, on: true },
        )
        .unwrap();
        assert_eq!(given, Ok(()));
        assert_eq!(db.member_role_ids("carol").unwrap(), vec![mods]);

        // Admin is not below bob's own highest role, so bob cannot give it.
        let refused = apply_role_change(
            &db,
            "bob",
            RoleChange::Member { user: "carol".into(), role: admin, on: true },
        )
        .unwrap();
        assert!(refused.is_err());
        // Dan has the same highest role as bob, so bob cannot change his roles.
        let refused = apply_role_change(
            &db,
            "bob",
            RoleChange::Member { user: "dan".into(), role: mods, on: false },
        )
        .unwrap();
        assert!(refused.is_err());
    }

    #[test]
    fn admins_cannot_give_permissions_they_lack() {
        let (db, _, _) = server();
        let refused = apply_role_change(
            &db,
            "bob",
            RoleChange::Create { name: "Owners".into(), color: String::new(), permissions: perm::ALL , mentionable: false, hoisted: false },
        )
        .unwrap();
        // Bob has MANAGE_ROLES and MANAGE_CHANNELS, not MANAGE_MESSAGES.
        assert!(refused.is_err());
        let fine = apply_role_change(
            &db,
            "bob",
            RoleChange::Create { name: "Cleaners".into(), color: String::new(), permissions: perm::MANAGE_CHANNELS , mentionable: false, hoisted: false },
        )
        .unwrap();
        assert_eq!(fine, Ok(()));
    }

    #[test]
    fn roles_are_checked_for_names_and_colours() {
        let (db, _, mods) = server();
        let taken = apply_role_change(
            &db,
            "owner",
            RoleChange::Create { name: "Mods".into(), color: String::new(), permissions: 0 , mentionable: false, hoisted: false },
        )
        .unwrap();
        assert!(taken.is_err());
        let bad_colour = apply_role_change(
            &db,
            "owner",
            RoleChange::Update { role: mods, name: "Mods".into(), color: "red".into(), permissions: 0 , mentionable: false, hoisted: false },
        )
        .unwrap();
        assert!(bad_colour.is_err());
    }

    #[test]
    fn deleting_a_role_takes_it_from_members() {
        let (db, _, mods) = server();
        apply_role_change(&db, "owner", RoleChange::Member { user: "carol".into(), role: mods, on: true })
            .unwrap()
            .unwrap();
        apply_role_change(&db, "owner", RoleChange::Delete { role: mods }).unwrap().unwrap();
        assert!(db.member_role_ids("carol").unwrap().is_empty());
        assert!(db.role_list().unwrap().iter().all(|r| r.id != mods));
    }

    #[test]
    fn admins_cannot_move_roles_above_their_own() {
        let (db, admin, mods) = server();
        // Bob tries to move Mods above Admin, which would put it above his own role.
        let refused = apply_role_change(&db, "bob", RoleChange::Move { role: mods, before: Some(admin) }).unwrap();
        assert!(refused.is_err());
        // Bob cannot move Admin at all: it is his own highest role.
        let refused = apply_role_change(&db, "bob", RoleChange::Move { role: admin, before: None }).unwrap();
        assert!(refused.is_err());
        // The owner can.
        let moved = apply_role_change(&db, "owner", RoleChange::Move { role: admin, before: None }).unwrap();
        assert_eq!(moved, Ok(()));
        let order: Vec<i64> = db.role_list().unwrap().iter().map(|r| r.id).filter(|&id| id != EVERYONE_ROLE).collect();
        assert_eq!(order, vec![mods, admin]);
    }

    #[test]
    fn only_the_owner_changes_the_owners_roles() {
        let (db, admin, mods) = server();
        // Bob outranks Mods, but the owner outranks everyone, so he may not change the owner's roles.
        let refused = apply_role_change(
            &db,
            "bob",
            RoleChange::Member { user: "owner".into(), role: mods, on: true },
        )
        .unwrap();
        assert!(refused.is_err());
        // The owner may give themselves a role, and it shows on the owner.
        let given = apply_role_change(
            &db,
            "owner",
            RoleChange::Member { user: "owner".into(), role: admin, on: true },
        )
        .unwrap();
        assert_eq!(given, Ok(()));
        assert_eq!(db.member_role_ids("owner").unwrap(), vec![admin]);
    }
}

#[cfg(test)]
mod everyone_tests {
    use super::*;

    fn server() -> Db {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("carol", "Carol", 2).unwrap();
        db
    }

    #[test]
    fn everyone_is_listed_last_and_cannot_be_deleted_moved_or_given() {
        let db = server();
        let order = db.role_list().unwrap();
        assert_eq!(order.last().map(|r| r.id), Some(EVERYONE_ROLE));
        let mods = db.create_role("Mods", "", 0, false, false).unwrap().unwrap();
        assert_eq!(db.role_list().unwrap().last().map(|r| r.id), Some(EVERYONE_ROLE), "new roles go above @everyone");
        assert!(apply_role_change(&db, "owner", RoleChange::Delete { role: EVERYONE_ROLE }).unwrap().is_err());
        assert!(apply_role_change(&db, "owner", RoleChange::Move { role: EVERYONE_ROLE, before: None }).unwrap().is_err());
        assert!(apply_role_change(&db, "owner", RoleChange::Member { user: "carol".into(), role: EVERYONE_ROLE, on: true }).unwrap().is_err());
        // Reordering the real roles leaves @everyone at the bottom.
        apply_role_change(&db, "owner", RoleChange::Move { role: mods, before: None }).unwrap().unwrap();
        assert_eq!(db.role_list().unwrap().last().map(|r| r.id), Some(EVERYONE_ROLE));
    }

    #[test]
    fn everyone_permissions_apply_to_every_member() {
        let db = server();
        let given = apply_role_change(
            &db,
            "owner",
            RoleChange::Update {
                role: EVERYONE_ROLE,
                name: "x".into(),
                color: "#112233".into(),
                permissions: perm::MANAGE_MESSAGES | perm::VIEW_CHANNEL | perm::SEND_MESSAGES,
                mentionable: false,
                hoisted: false,
            },
        )
        .unwrap();
        assert_eq!(given, Ok(()));
        let everyone = db.role_list().unwrap().into_iter().find(|r| r.id == EVERYONE_ROLE).unwrap();
        assert_eq!(everyone.name, "@everyone", "its name is fixed");
        assert_eq!(everyone.color, "#112233");
        let order = db.role_list().unwrap();
        // Carol has no roles at all, but still holds @everyone's permissions.
        let carol = power_of(&db, "carol", &order).unwrap();
        assert!(carol.has(perm::MANAGE_MESSAGES));
        assert!(!carol.has(perm::MANAGE_CHANNELS));
        assert!(carol.top.is_none());
    }

    #[test]
    fn administrator_has_everything_even_when_a_channel_denies_it() {
        let db = server();
        db.add_member("erin", "Erin", 5).unwrap();
        let general = db.ensure_channel("general").unwrap();
        db.set_channel_override(general, EVERYONE_ROLE, None, None).unwrap();
        let admins = db.create_role("Administrators", "", perm::ADMINISTRATOR, false, false).unwrap().unwrap();
        db.set_member_role("erin", admins, true).unwrap();
        db.set_channel_override(general, admins, Some(false), Some(false)).unwrap();
        // The denial is for a role erin holds, but administrators can still see and send.
        let erin = Access::new(&db, "erin").unwrap();
        assert!(erin.allows(general, perm::VIEW_CHANNEL));
        assert!(erin.allows(general, perm::SEND_MESSAGES));
        assert!(erin.power.has(perm::MANAGE_ROLES));
        // An administrator may give out any permission, since they have all of them.
        let order = db.role_list().unwrap();
        let power = power_of(&db, "erin", &order).unwrap();
        assert_eq!(power.grantable(), perm::ALL);
    }

    #[test]
    fn everyone_keeps_channel_permissions_until_another_role_has_them() {
        let db = server();
        let everyone = |permissions: u32| RoleChange::Update {
            role: EVERYONE_ROLE,
            name: String::new(),
            color: String::new(),
            permissions,
            mentionable: false,
            hoisted: false,
        };
        // @everyone starts with both channel permissions.
        let start = db.role_list().unwrap().into_iter().find(|r| r.id == EVERYONE_ROLE).unwrap();
        assert_eq!(start.permissions & (perm::VIEW_CHANNEL | perm::SEND_MESSAGES), perm::VIEW_CHANNEL | perm::SEND_MESSAGES);

        // Nobody else holds them yet, so they cannot be taken away.
        assert_eq!(apply_role_change(&db, "owner", everyone(0)).unwrap(), Err(EVERYONE_KEEPS.into()));
        assert_eq!(apply_role_change(&db, "owner", everyone(perm::VIEW_CHANNEL)).unwrap(), Err(EVERYONE_KEEPS.into()));

        // Once another role holds View channels, @everyone may drop it, but not Message channels.
        db.create_role("Viewers", "", perm::VIEW_CHANNEL, false, false).unwrap().unwrap();
        assert_eq!(apply_role_change(&db, "owner", everyone(perm::SEND_MESSAGES)).unwrap(), Ok(()));
        assert_eq!(apply_role_change(&db, "owner", everyone(0)).unwrap(), Err(EVERYONE_KEEPS.into()));
    }
}
