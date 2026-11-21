mod card;
mod db;
mod discovery;
mod web;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{
    perm, AuthReply, AuthRequest, ChannelRef, ChatMessage, ClientMsg, RoleInfo, ServerMsg, UserStatus,
    DEFAULT_ADDR, DEFAULT_AUTH_ADDR, DISCOVERY_PORT, EVERYONE_ROLE, MAX_ROLE_NAME_LEN, MAX_SERVER_DESCRIPTION_LEN,
    MAX_SERVER_ICON_LEN,
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
    /// The (member, name) pairs AutoMod has already posted an alert about, so each is reported once.
    name_alerts: std::collections::HashSet<(String, String)>,
    /// Mentions each member's recent messages made, as (when, how many), for mention raid detection.
    mention_log: HashMap<String, std::collections::VecDeque<(u64, usize)>>,
    /// Members AutoMod has timed out from sending, and until when (Unix seconds).
    timeouts: HashMap<String, u64>,
    /// When each member last sent a message in each channel with slow mode, as (channel id, member id)
    /// to Unix seconds.
    slow_last: HashMap<(i64, String), u64>,
}

impl State {
    /// True while a peer removed for not answering is still in its cooldown.
    fn recently_dead(&self, addr: &str) -> bool {
        self.dead_peers
            .get(addr)
            .is_some_and(|at| at.elapsed() < discovery::DEAD_PEER_COOLDOWN)
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
        let statuses = self.db.statuses().unwrap_or_default();
        let badges = self.db.all_member_badges().unwrap_or_default();
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
                let status = statuses.get(&id).copied().unwrap_or_default();
                // Invisible members are shown as offline, even while connected.
                let online = online && status != common::PresenceStatus::Invisible;
                let badges = badges.get(&id).cloned().unwrap_or_default();
                UserStatus {
                    id,
                    name,
                    online,
                    owner,
                    roles,
                    joined,
                    status,
                    badges,
                }
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
    /// Where to serve the web client, if this server serves it.
    web_addr: Option<String>,
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
            db_path: if count == 1 {
                db_path.clone()
            } else {
                numbered_path(&db_path, server_port)
            },
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
            // The first server of a group serves the web client: on VAXELIUM_WEB_ADDR (host:port, or
            // "off"), or by default on the first port past the group, which is 7880 for one server on 7878.
            web_addr: if i == 0 {
                let default = port.checked_add(count + 1).map(|p| format!("{host}:{p}"));
                match std::env::var("VAXELIUM_WEB_ADDR") {
                    Ok(v) if v.eq_ignore_ascii_case("off") => None,
                    Ok(v) => Some(v),
                    Err(_) => default,
                }
            } else {
                None
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
        web_addr,
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
        name_alerts: std::collections::HashSet::new(),
        mention_log: HashMap::new(),
        timeouts: HashMap::new(),
        slow_last: HashMap::new(),
    }));
    {
        let s = state.lock().unwrap();
        for peer in peers
            .iter()
            .filter(|a| discovery::valid_addr(a) && a.as_str() != advertise)
        {
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
                println!(
                    "updated {} member id(s) to their new ids ({changed} references)",
                    pairs.len()
                );
            }
            Ok(_) => eprintln!("unexpected reply from the auth service about member ids"),
            Err(e) => {
                eprintln!("could not update member ids yet ({e}); will try again on the next start")
            }
        }
    }

    if let Some(web_addr) = web_addr {
        let (state, tx, auth_addr) = (state.clone(), tx.clone(), auth_addr.clone());
        tokio::spawn(async move {
            if let Err(e) = web::serve(web_addr, state, tx, auth_addr).await {
                eprintln!("the web client could not start: {e}");
            }
        });
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
        _ => {}
    }

    // Handshake: otherwise the first message must be a Connect carrying a session token.
    let session_token: String;
    let (user, mut display) = match serde_json::from_str::<ClientMsg>(&first) {
        Ok(ClientMsg::Connect { token, join }) => match check_session(&state, &auth_addr, &token).await {
            Ok(identity) => {
                let (deleted, banned, left, kicked) = {
                    let s = state.lock().unwrap();
                    let left = s.db.has_left(&identity.id)?;
                    if left && join {
                        s.db.clear_left(&identity.id)?;
                    }
                    let left = left && !join;
                    let kicked = if left { s.db.kicked_reason(&identity.id)? } else { None };
                    (s.db.is_deleted()?, s.db.is_banned(&identity.id)?, left, kicked)
                };
                if deleted {
                    send(&mut writer, &ServerMsg::Error { text: "this server has been deleted".into() }).await?;
                    return Ok(());
                }
                // A ban is not undone by asking to join again. Only an administrator can lift it.
                if banned {
                    send(&mut writer, &ServerMsg::Error { text: "you are banned from this server".into() }).await?;
                    return Ok(());
                }
                if left {
                    // A kicked account is told so; one that left is told it left.
                    let reply = match kicked {
                        Some(reason) => ServerMsg::Kicked { reason },
                        None => ServerMsg::NotJoined,
                    };
                    send(&mut writer, &reply).await?;
                    return Ok(());
                }
                session_token = token;
                (identity.id, identity.name)
            }
            Err(text) => {
                send(&mut writer, &ServerMsg::Error { text }).await?;
                return Ok(());
            }
        },
        _ => {
            send(
                &mut writer,
                &ServerMsg::Error {
                    text: "connect with a session token first".into(),
                },
            )
            .await?;
            return Ok(());
        }
    };

    // The same account may be connected more than once (for example from two windows). Each
    // connection adds one entry to `online`, and the member shows as online while any is open.
    let (users, server_name, server_description, server_icon, first_join) = {
        let mut s = state.lock().unwrap();
        // A member joins once: a later connect is not a new join.
        let first_join = !s.db.is_member(&user)?;
        s.db.add_member(&user, &display, now_secs())?;
        s.online.push(user.clone());
        (
            s.presence(),
            s.db.server_name()?,
            s.db.server_description()?,
            s.db.server_icon()?,
            first_join,
        )
    };

    // No channel is open until the client joins one. Joining creates a channel only when it is asked for by name.
    let mut channel = String::new();
    let mut channel_id: i64 = 0;
    // When this connection's recent messages were sent, for the spam filter.
    let mut recent_sends: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
    // Removes the user and broadcasts presence on every exit path, including errors.
    let _online = OnlineGuard {
        state: state.clone(),
        tx: tx.clone(),
        user: user.clone(),
    };
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
    let badges = state.lock().unwrap().db.badge_list()?;
    send(&mut writer, &ServerMsg::Badges { badges }).await?;
    let overrides = state.lock().unwrap().db.channel_overrides()?;
    send(&mut writer, &ServerMsg::ChannelOverrides { overrides }).await?;
    let modes = state.lock().unwrap().db.slow_modes()?;
    send(&mut writer, &ServerMsg::SlowModes { modes }).await?;
    let counts = unread_counts(&state, &user)?;
    send(&mut writer, &ServerMsg::Unread { counts }).await?;
    // A member who is still timed out is told, so the client can show it from the start.
    let timed_out = state.lock().unwrap().db.active_timeout(&user, now_secs())?;
    if let Some((until, reason)) = timed_out {
        send(&mut writer, &ServerMsg::TimedOut { user: user.clone(), until, reason }).await?;
        arm_timeout_end(&state, &tx, &user, until);
    }
    let overrides = state.lock().unwrap().db.category_overrides()?;
    send(&mut writer, &ServerMsg::CategoryOverrides { overrides }).await?;
    let (channels, categories) = {
        let s = state.lock().unwrap();
        (s.db.channel_member_overrides()?, s.db.category_member_overrides()?)
    };
    send(&mut writer, &ServerMsg::MemberOverrides { channels, categories }).await?;
    {
        // Lets others message this account from another server. Does not hold up the welcome.
        let own = state.lock().unwrap().advertise.clone();
        let (state2, auth2, token2) = (state.clone(), auth_addr.to_string(), session_token.clone());
        tokio::spawn(async move {
            let _ = auth_call(
                &state2,
                &auth2,
                &AuthRequest::Seen {
                    token: token2,
                    addr: own,
                },
            )
            .await;
        });
    }
    let _ = tx.send(ServerMsg::Presence { users });
    // The join notice is saved in its channel, so everyone sees it in the history, and it is sent
    // live to those who have that channel open. Without a channel set, it goes to the first channel.
    // It is only posted when the member first joins, not on every connect.
    let system = state.lock().unwrap().db.system_messages()?;
    if system.connect_notice && first_join {
        let notice = {
            let s = state.lock().unwrap();
            let target = match system.channel_id {
                Some(id) => Some(id),
                None => s.db.channel_list()?.first().map(|c| c.id),
            };
            match target {
                Some(id) => s.db.channel_name(id)?.map(|name| (id, name)),
                None => None,
            }
        };
        if let Some((notice_channel, channel)) = notice {
            // Servers that had the notice on before the System role existed get the role now.
            state.lock().unwrap().db.ensure_system_role()?;
            // The welcome card: the member's avatar, the server's name and how many members it has. If the
            // avatar or the card can't be had, the notice is just its text.
            let (server_name, count) = {
                let s = state.lock().unwrap();
                (s.db.server_name()?, s.db.member_names()?.len() as u64)
            };
            let (avatar, banner) = match auth_call(&state, &auth_addr, &AuthRequest::ProfileCard { id: user.clone(), token: String::new() }).await {
                Ok(AuthReply::ProfileCard { avatar, banner, .. }) => {
                    let picture = if avatar.is_empty() {
                        None
                    } else {
                        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, avatar).ok()
                    };
                    (picture, banner)
                }
                _ => (None, String::new()),
            };
            let member = card::Member { names: vec![display.clone()], avatar, banner };
            let picture = tokio::task::spawn_blocking(move || card::welcome(&member, &server_name, count)).await.ok().flatten();
            let image = picture.map(|png| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png));
            let mut message = ChatMessage {
                id: 0,
                channel_id: notice_channel,
                channel,
                author: "System".into(),
                author_id: common::notice_author(&user),
                text: format!("{display} joined the server"),
                timestamp: now_secs(),
                edited: false,
                reactions: Vec::new(),
                reply: None,
                pinned: false,
                report: None,
                image,
            };
            message.id = state.lock().unwrap().db.save_message(&message)?;
            let _ = tx.send(ServerMsg::Message { message });
        }
    }

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
                        if !may_in_category(&state, &user, category, perm::MANAGE_CHANNELS)? {
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

                    ClientMsg::RenameCategory { category, new_name } => {
                        if !may_in_category(&state, &user, category, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let name = new_name.trim().to_string();
                        if name.is_empty() || name.chars().count() > MAX_CATEGORY_NAME_LEN {
                            send(&mut writer, &ServerMsg::Error {
                                text: format!("category name must be 1-{MAX_CATEGORY_NAME_LEN} characters"),
                            })
                            .await?;
                            continue;
                        }
                        let renamed = state.lock().unwrap().db.rename_category(category, &name)?;
                        if renamed {
                            broadcast_categories(&state, &tx)?;
                        } else {
                            send(&mut writer, &ServerMsg::Error {
                                text: "that category cannot be renamed to that name".into(),
                            })
                            .await?;
                        }
                    }

                    ClientMsg::RenameChannel { channel: id, new_name } => {
                        if !may_in_channel(&state, &user, id, perm::MANAGE_CHANNELS)? {
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
                        if !may_in_channel(&state, &user, id, perm::MANAGE_CHANNELS)? {
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
                        if !may_in_channel(&state, &user, id, perm::MANAGE_CHANNELS)? {
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
                        if !may_change_category(&state, &user, id, category)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CATEGORY_LEAVE.into() }).await?;
                            continue;
                        }
                        state.lock().unwrap().db.place_channel(id, category, before)?;
                        broadcast_categories(&state, &tx)?;
                        let overrides = state.lock().unwrap().db.channel_overrides()?;
                        let _ = tx.send(ServerMsg::ChannelOverrides { overrides });
                        send_member_overrides(&state, &tx)?;
                    }

                    ClientMsg::MoveChannel { channel: id, category } => {
                        if !may_in_channel(&state, &user, id, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let current = state.lock().unwrap().db.channel_name(id)?;
                        let movable = matches!(&current, Some(name) if !name.starts_with("dm-"));
                        if !movable {
                            send(&mut writer, &ServerMsg::Error { text: "that channel cannot be moved".into() }).await?;
                            continue;
                        }
                        if !may_change_category(&state, &user, id, category)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CATEGORY_LEAVE.into() }).await?;
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
                            Ok(()) => {
                                broadcast_categories(&state, &tx)?;
                                let overrides = state.lock().unwrap().db.channel_overrides()?;
                                let _ = tx.send(ServerMsg::ChannelOverrides { overrides });
                                send_member_overrides(&state, &tx)?;
                            }
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

                    ClientMsg::CreateBadge { name, color } => {
                        if let Some(text) = change_badges(&state, &tx, &user, BadgeChange::Create { name, color })? {
                            send(&mut writer, &ServerMsg::Error { text }).await?;
                        }
                    }

                    ClientMsg::DeleteBadge { badge } => {
                        if let Some(text) = change_badges(&state, &tx, &user, BadgeChange::Delete { badge })? {
                            send(&mut writer, &ServerMsg::Error { text }).await?;
                        }
                    }

                    ClientMsg::SetMemberBadge { user: member, badge, on } => {
                        if let Some(text) = change_badges(&state, &tx, &user, BadgeChange::Member { user: member, badge, on })? {
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
                        let allowed = may(&state, &user, perm::MANAGE_SERVER)?;
                        let result = {
                            let s = state.lock().unwrap();
                            if allowed {
                                s.db.set_server_icon(png_base64.as_deref())?;
                                Ok(())
                            } else {
                                Err("you need the Manage Server permission to change this server")
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
                        let allowed = may(&state, &user, perm::MANAGE_SERVER)?;
                        let result = {
                            let s = state.lock().unwrap();
                            if allowed {
                                s.db.set_server_name(&name)?;
                                s.db.set_server_description(&description)?;
                                Ok(())
                            } else {
                                Err("you need the Manage Server permission to change this server")
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
                                text: "you need the Manage Channels permission to create a channel".into(),
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
                                // Opening a channel reads it.
                                state.lock().unwrap().db.mark_read(&user, id)?;
                                send(&mut writer, &ServerMsg::Joined { channel: ChannelRef { id, name }, history }).await?;
                            }
                            None => {
                                send(&mut writer, &ServerMsg::Error { text: "that direct message is not yours".into() }).await?;
                            }
                        }
                    }

                    ClientMsg::Say { text, reply_to } => {
                        // A flagged name is reported, and refused if the rule blocks, only when its member
                        // tries to send a message.
                        if let Some(word) = name_flag(&state, &user, &display)? {
                            if let Some(reason) = name_block(&state, &user, &display)? {
                                let why = format!("their name matches “{word}”");
                                // The name was the problem, not the message, so the message is left out.
                                report_block(&state, &tx, &user, &display, channel_id, "", &why, "Flagged Member Names", &word)?;
                                send(&mut writer, &ServerMsg::Error { text: reason }).await?;
                                continue;
                            }
                            name_alert(&state, &tx, &user, &display, &word)?;
                        }
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
                        let now = now_secs();
                        let timed_out = state.lock().unwrap().db.active_timeout(&user, now)?;
                        if let Some((until, _)) = timed_out {
                            send(&mut writer, &ServerMsg::Error { text: timeout_notice(until, now) }).await?;
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
                        // @everyone and @here notify the whole channel, so they need the Mention Everyone permission.
                        if common::everyone_mentioned(&text) && !may_in_channel(&state, &user, channel_id, perm::MENTION_EVERYONE)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_EVERYONE_PERMISSION.into() }).await?;
                            continue;
                        }
                        // Slow mode: each member waits this long after their last message in the channel. Members
                        // with the Bypass Slow Mode permission (or Manage Messages) there are not held to it.
                        let slow = state.lock().unwrap().db.slow_mode(channel_id)?;
                        if slow > 0
                            && !may_in_channel(&state, &user, channel_id, perm::BYPASS_SLOW_MODE)?
                            && !may_in_channel(&state, &user, channel_id, perm::MANAGE_MESSAGES)?
                        {
                            let now = now_secs();
                            let last = state.lock().unwrap().slow_last.get(&(channel_id, user.clone())).copied();
                            if let Some(last) = last.filter(|&last| now.saturating_sub(last) < slow) {
                                let wait = slow - now.saturating_sub(last);
                                send(&mut writer, &ServerMsg::Error {
                                    text: format!("slow mode: you can send again in {wait} seconds"),
                                })
                                .await?;
                                continue;
                            }
                        }
                        if let Some(refusal) = automod_refusal(&state, &user, channel_id, &text, Some(&mut recent_sends))? {
                            if refusal.alert {
                                report_block(
                                    &state,
                                    &tx,
                                    &user,
                                    &display,
                                    channel_id,
                                    &text,
                                    &refusal.reason,
                                    refusal.rule,
                                    &refusal.keyword,
                                )?;
                            }
                            if refusal.block {
                                let shown = if refusal.notice.is_empty() { refusal.reason } else { refusal.notice };
                                send(&mut writer, &ServerMsg::Error { text: shown }).await?;
                                continue;
                            }
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
                            reply: None,
                            pinned: false,
                            report: None,
                            image: None,
                        };
                        // A reply must answer a message in the same channel; anything else is sent plain.
                        if let Some(id) = reply_to {
                            message.reply = state.lock().unwrap().db.reply_ref(id, channel_id)?;
                        }
                        message.id = state.lock().unwrap().db.save_message(&message)?;
                        if slow > 0 {
                            state.lock().unwrap().slow_last.insert((channel_id, user.clone()), now_secs());
                        }
                        let _ = tx.send(ServerMsg::Message { message });
                    }
                    ClientMsg::SetProfileStatus { text } => {
                        let text: String = text.trim().chars().take(common::MAX_STATUS_TEXT_LEN).collect();
                        let _ = tx.send(ServerMsg::ProfileStatus { user: user.clone(), text });
                    }
                    ClientMsg::SetDisplayName { name } => {
                        let name = name.trim().to_string();
                        if name.is_empty() || name.chars().count() > common::MAX_DISPLAY_NAME_LEN {
                            send(&mut writer, &ServerMsg::Error {
                                text: format!("name must be 1-{} characters", common::MAX_DISPLAY_NAME_LEN),
                            })
                            .await?;
                            continue;
                        }
                        // Saved for everyone, then every connection of this account is told, so its sends use the
                        // new name. The member list is sent again with it.
                        {
                            let s = state.lock().unwrap();
                            s.db.add_member(&user, &name, now_secs())?;
                            let _ = tx.send(ServerMsg::MemberRenamed { id: user.clone(), name: name.clone() });
                            let _ = tx.send(ServerMsg::Presence { users: s.presence() });
                        }
                        display = name;
                    }
                    ClientMsg::SetStatus { status } => {
                        let users = {
                            let s = state.lock().unwrap();
                            s.db.set_status(&user, status)?;
                            s.presence()
                        };
                        let _ = tx.send(ServerMsg::Presence { users });
                    }

                    ClientMsg::Typing => {
                        // Only the members who may view the channel hear about it.
                        let viewable = channel_id != 0 && Access::new(&state.lock().unwrap().db, &user)?.allows(channel_id, perm::VIEW_CHANNEL);
                        if viewable {
                            let _ = tx.send(ServerMsg::Typing { channel_id, user: user.clone(), name: display.clone() });
                        }
                    }

                    ClientMsg::SetSlowMode { channel: id, seconds } => {
                        if !may_in_channel(&state, &user, id, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let result = {
                            let s = state.lock().unwrap();
                            match s.db.channel_name(id)? {
                                None => Err("there is no such channel".to_string()),
                                Some(name) if name.starts_with("dm-") => Err("direct messages can't have slow mode".to_string()),
                                Some(_) if seconds > common::MAX_SLOW_MODE_SECS => Err(format!(
                                    "slow mode can be at most {} hours",
                                    common::MAX_SLOW_MODE_SECS / 3600
                                )),
                                Some(_) => {
                                    s.db.set_slow_mode(id, seconds)?;
                                    Ok(s.db.slow_modes()?)
                                }
                            }
                        };
                        match result {
                            Ok(modes) => {
                                let _ = tx.send(ServerMsg::SlowModes { modes });
                            }
                            Err(text) => send(&mut writer, &ServerMsg::Error { text }).await?,
                        }
                    }

                    ClientMsg::SetChannelOverride { channel, role, view, send: send_setting, manage_channels, manage_messages } => {
                        if !may_in_channel(&state, &user, channel, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: "you need the Manage Channels permission for that".into() }).await?;
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
                                    s.db.set_channel_override(channel, role, common::Setting { view, send: send_setting, manage_channels, manage_messages })?;
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

                    ClientMsg::SetCategoryMemberOverride { category, member, view, send: send_setting, manage_channels, manage_messages } => {
                        if !may_in_category(&state, &user, category, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let result: Result<(Vec<common::MemberChannelOverride>, Vec<common::MemberCategoryOverride>), String> = {
                            let s = state.lock().unwrap();
                            if !s.db.category_exists(category)? {
                                Err("there is no such category".to_string())
                            } else if !s.db.is_member(&member)? {
                                Err("there is no such member".to_string())
                            } else {
                                s.db.member_category_override(category, &member, common::Setting { view, send: send_setting, manage_channels, manage_messages })?;
                                Ok((s.db.channel_member_overrides()?, s.db.category_member_overrides()?))
                            }
                        };
                        match result {
                            Ok((channels, categories)) => {
                                let _ = tx.send(ServerMsg::MemberOverrides { channels, categories });
                            }
                            Err(text) => send(&mut writer, &ServerMsg::Error { text }).await?,
                        }
                    }

                    ClientMsg::SetChannelMemberOverride { channel, member, view, send: send_setting, manage_channels, manage_messages } => {
                        if !may_in_channel(&state, &user, channel, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let result: Result<(Vec<common::MemberChannelOverride>, Vec<common::MemberCategoryOverride>), String> = {
                            let s = state.lock().unwrap();
                            match s.db.channel_name(channel)? {
                                None => Err("there is no such channel".to_string()),
                                Some(name) if name.starts_with("dm-") => Err("direct messages can't have channel permissions".to_string()),
                                Some(_) if !s.db.is_member(&member)? => Err("there is no such member".to_string()),
                                Some(_) => {
                                    s.db.member_channel_override(channel, &member, common::Setting { view, send: send_setting, manage_channels, manage_messages })?;
                                    Ok((s.db.channel_member_overrides()?, s.db.category_member_overrides()?))
                                }
                            }
                        };
                        match result {
                            Ok((channels, categories)) => {
                                let _ = tx.send(ServerMsg::MemberOverrides { channels, categories });
                            }
                            Err(text) => send(&mut writer, &ServerMsg::Error { text }).await?,
                        }
                    }

                    ClientMsg::SetCategoryOverride { category, role, view, send: send_setting, manage_channels, manage_messages } => {
                        if !may_in_category(&state, &user, category, perm::MANAGE_CHANNELS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_CHANNEL_PERMISSION.into() }).await?;
                            continue;
                        }
                        let result: Result<(Vec<common::CategoryOverride>, Vec<common::ChannelOverride>), String> = {
                            let s = state.lock().unwrap();
                            if !s.db.category_exists(category)? {
                                Err("there is no such category".to_string())
                            } else if role != EVERYONE_ROLE && !s.db.role_list()?.iter().any(|r| r.id == role) {
                                Err("there is no such role".to_string())
                            } else {
                                s.db.set_category_override(category, role, common::Setting { view, send: send_setting, manage_channels, manage_messages })?;
                                Ok((s.db.category_overrides()?, s.db.channel_overrides()?))
                            }
                        };
                        match result {
                            Ok((categories, channels)) => {
                                // The channels in the category changed too, so their overrides are sent with it.
                                let _ = tx.send(ServerMsg::ChannelOverrides { overrides: channels });
                                let _ = tx.send(ServerMsg::CategoryOverrides { overrides: categories });
                            }
                            Err(text) => send(&mut writer, &ServerMsg::Error { text }).await?,
                        }
                    }

                    ClientMsg::React { id, emoji } => {
                        // Blocked without a notice: AutoMod only speaks up when a message is sent.
                        if name_block(&state, &user, &display)?.is_some() {
                            continue;
                        }
                        if !message_visible(&state, &user, id)? {
                            send(&mut writer, &ServerMsg::Error { text: CANNOT_VIEW.into() }).await?;
                            continue;
                        }
                        if !common::is_reaction(&emoji) {
                            send(&mut writer, &ServerMsg::Error { text: "that reaction is not allowed".into() }).await?;
                            continue;
                        }
                        let allowed = {
                            let s = state.lock().unwrap();
                            match s.db.message_channel(id)? {
                                Some(c) if c.starts_with("dm-") => s.db.is_dm_member(&c, &user)?,
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
                        // The author may delete a message, and so may anyone who may manage messages in its channel.
                        let channel = state.lock().unwrap().db.message_channel_id(id)?;
                        let any = match channel {
                            Some(channel) => may_in_channel(&state, &user, channel, perm::MANAGE_MESSAGES)?,
                            None => false,
                        };
                        let deleted = state.lock().unwrap().db.delete_message(id, &user, any)?;
                        match deleted {
                            Some(channel_id) => {
                                let _ = tx.send(ServerMsg::MessageDeleted { channel_id, id });
                            }
                            None => {
                                send(&mut writer, &ServerMsg::Error {
                                    text: "only the author, or someone with Manage Messages, can delete a message".into(),
                                })
                                .await?;
                            }
                        }
                    }
                    ClientMsg::GetAutoMod => {
                        if !may(&state, &user, perm::MANAGE_SERVER)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_AUTOMOD_PERMISSION.into() }).await?;
                            continue;
                        }
                        let rules = state.lock().unwrap().db.automod()?;
                        send(&mut writer, &ServerMsg::AutoMod { rules }).await?;
                    }
                    ClientMsg::SetAutoMod { rules } => {
                        if !may(&state, &user, perm::MANAGE_SERVER)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_AUTOMOD_PERMISSION.into() }).await?;
                            continue;
                        }
                        let rules = rules.tidied();
                        {
                            let s = state.lock().unwrap();
                            s.db.set_automod(&rules)?;
                            // The AutoMod role is made once a rule is on, so the bot's messages have a colour.
                            if rules.any_rule_on() {
                                s.db.ensure_automod_role()?;
                                let _ = tx.send(ServerMsg::Roles { roles: s.db.role_list()? });
                            }
                        }
                        send(&mut writer, &ServerMsg::AutoMod { rules }).await?;
                    }
                    ClientMsg::TransferOwnership { id } => {
                        let (owner, member) = {
                            let s = state.lock().unwrap();
                            (s.db.owner()?, s.db.is_member(&id)?)
                        };
                        if owner.as_deref() != Some(user.as_str()) {
                            send(&mut writer, &ServerMsg::Error { text: "only the owner can make someone else the owner".into() })
                                .await?;
                            continue;
                        }
                        if !member {
                            send(&mut writer, &ServerMsg::Error { text: "that person is not a member of this server".into() })
                                .await?;
                            continue;
                        }
                        let users = {
                            let s = state.lock().unwrap();
                            s.db.transfer_ownership(&id)?;
                            s.presence()
                        };
                        let _ = tx.send(ServerMsg::Presence { users });
                    }
                    ClientMsg::DeleteServer => {
                        let owner = state.lock().unwrap().db.owner()?;
                        if owner.as_deref() != Some(user.as_str()) {
                            send(&mut writer, &ServerMsg::Error { text: "only the owner can delete the server".into() })
                                .await?;
                            continue;
                        }
                        state.lock().unwrap().db.delete_server()?;
                        // Every connection, this one included, is told and closes.
                        let _ = tx.send(ServerMsg::ServerDeleted);
                    }
                    ClientMsg::Kick { user: member, reason } => {
                        if let Err(text) = remove_member(&state, &tx, &user, &member, &clean_reason(reason), false)? {
                            send(&mut writer, &ServerMsg::Error { text }).await?;
                        }
                    }

                    ClientMsg::Ban { user: member, reason } => {
                        if let Err(text) = remove_member(&state, &tx, &user, &member, &clean_reason(reason), true)? {
                            send(&mut writer, &ServerMsg::Error { text }).await?;
                        }
                    }

                    ClientMsg::Unban { user: member } => {
                        if !may(&state, &user, perm::BAN_MEMBERS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_BAN_PERMISSION.into() }).await?;
                            continue;
                        }
                        state.lock().unwrap().db.unban(&member)?;
                    }

                    ClientMsg::Timeout { user: member, seconds, reason } => {
                        if let Err(text) = timeout_member(&state, &tx, &user, &member, seconds, &clean_reason(reason))? {
                            send(&mut writer, &ServerMsg::Error { text }).await?;
                        }
                    }

                    ClientMsg::MarkRead { channel_id: read } => {
                        state.lock().unwrap().db.mark_read(&user, read)?;
                    }

                    ClientMsg::RemoveTimeout { user: member } => {
                        if !may(&state, &user, perm::TIMEOUT_MEMBERS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_TIMEOUT_PERMISSION.into() }).await?;
                            continue;
                        }
                        if state.lock().unwrap().db.clear_timeout(&member)? {
                            let _ = tx.send(ServerMsg::TimeoutEnded { user: member });
                        } else {
                            send(&mut writer, &ServerMsg::Error { text: "that member is not timed out".into() }).await?;
                        }
                    }

                    ClientMsg::GetBans => {
                        if !may(&state, &user, perm::BAN_MEMBERS)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_BAN_PERMISSION.into() }).await?;
                            continue;
                        }
                        let bans = state.lock().unwrap().db.bans()?;
                        send(&mut writer, &ServerMsg::Bans { bans }).await?;
                    }

                    ClientMsg::LeaveServer => {
                        let owner = state.lock().unwrap().db.info_value("owner")?;
                        if owner.as_deref() == Some(user.as_str()) {
                            send(&mut writer, &ServerMsg::Error { text: "the owner cannot leave the server".into() })
                                .await?;
                            continue;
                        }
                        let users = {
                            let s = state.lock().unwrap();
                            s.db.leave_server(&user)?;
                            s.presence()
                        };
                        // Everyone's member list drops the leaver now, not when this connection closes.
                        let _ = tx.send(ServerMsg::Presence { users });
                        send(&mut writer, &ServerMsg::Left).await?;
                        // Ends this connection. Its presence is sent as it closes, with the account gone.
                        break;
                    }
                    ClientMsg::GetSystemMessages => {
                        if !may(&state, &user, perm::MANAGE_SERVER)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_SYSTEM_PERMISSION.into() }).await?;
                            continue;
                        }
                        let settings = state.lock().unwrap().db.system_messages()?;
                        send(&mut writer, &ServerMsg::SystemMessages { settings }).await?;
                    }
                    ClientMsg::SetSystemMessages { settings } => {
                        if !may(&state, &user, perm::MANAGE_SERVER)? {
                            send(&mut writer, &ServerMsg::Error { text: NO_SYSTEM_PERMISSION.into() }).await?;
                            continue;
                        }
                        // The lock is released before anything is sent.
                        let missing = {
                            let s = state.lock().unwrap();
                            match settings.channel_id {
                                Some(id) if s.db.channel_name(id)?.is_none() => true,
                                _ => {
                                    s.db.set_system_messages(&settings)?;
                                    false
                                }
                            }
                        };
                        if missing {
                            send(&mut writer, &ServerMsg::Error { text: "that channel does not exist".into() }).await?;
                            continue;
                        }
                        // The System role is made once the connect notice is on, the way the AutoMod role is made
                        // once a rule is on.
                        if settings.connect_notice {
                            let s = state.lock().unwrap();
                            s.db.ensure_system_role()?;
                            let _ = tx.send(ServerMsg::Roles { roles: s.db.role_list()? });
                        }
                        send(&mut writer, &ServerMsg::SystemMessages { settings }).await?;
                    }
                    ClientMsg::Pin { id, pinned } => {
                        // Blocked without a notice: AutoMod only speaks up when a message is sent.
                        if name_block(&state, &user, &display)?.is_some() {
                            continue;
                        }
                        if !message_visible(&state, &user, id)? {
                            send(&mut writer, &ServerMsg::Error { text: CANNOT_VIEW.into() }).await?;
                            continue;
                        }
                        // Either member of a direct message may pin in it; in a channel it takes Pin messages
                        // (or Manage messages).
                        let (channel, in_dm) = {
                            let s = state.lock().unwrap();
                            let name = s.db.message_channel(id)?.unwrap_or_default();
                            (s.db.message_channel_id(id)?, name.starts_with("dm-"))
                        };
                        let Some(channel) = channel else { continue };
                        let may_pin = in_dm
                            || may_in_channel(&state, &user, channel, perm::PIN_MESSAGES)?
                            || may_in_channel(&state, &user, channel, perm::MANAGE_MESSAGES)?;
                        if !may_pin {
                            send(&mut writer, &ServerMsg::Error {
                                text: "you need the Pin Messages permission to pin messages".into(),
                            })
                            .await?;
                            continue;
                        }
                        let result = state.lock().unwrap().db.set_pinned(id, pinned, now_secs())?;
                        match result {
                            Ok(Some(channel_id)) => {
                                let _ = tx.send(ServerMsg::MessagePinned { channel_id, id, pinned });
                            }
                            Ok(None) => {}
                            Err(db::PinsFull) => {
                                send(&mut writer, &ServerMsg::Error {
                                    text: format!("a channel can have at most {} pinned messages", common::MAX_PINS),
                                })
                                .await?;
                            }
                        }
                    }
                    ClientMsg::ListPins { channel } => {
                        let allowed = Access::new(&state.lock().unwrap().db, &user)?.allows(channel, perm::VIEW_CHANNEL);
                        if !allowed {
                            send(&mut writer, &ServerMsg::Error { text: CANNOT_VIEW.into() }).await?;
                            continue;
                        }
                        let messages = state.lock().unwrap().db.pins(channel)?;
                        send(&mut writer, &ServerMsg::Pins { channel_id: channel, messages }).await?;
                    }
                    ClientMsg::Edit { id, text } => {
                        if let Some(reason) = name_block(&state, &user, &display)? {
                            let channel = state.lock().unwrap().db.message_channel_id(id)?.unwrap_or(0);
                            let word = name_flag(&state, &user, &display)?.unwrap_or_default();
                            report_block(
                                &state,
                                &tx,
                                &user,
                                &display,
                                channel,
                                "",
                                &format!("their name matches “{word}”"),
                                "Flagged Member Names",
                                &word,
                            )?;
                            send(&mut writer, &ServerMsg::Error { text: reason }).await?;
                            continue;
                        }
                        if !message_visible(&state, &user, id)? {
                            send(&mut writer, &ServerMsg::Error { text: CANNOT_VIEW.into() }).await?;
                            continue;
                        }
                        let text = text.trim().to_string();
                        if common::everyone_mentioned(&text) {
                            let channel = state.lock().unwrap().db.message_channel_id(id)?.unwrap_or(0);
                            if !may_in_channel(&state, &user, channel, perm::MENTION_EVERYONE)? {
                                send(&mut writer, &ServerMsg::Error { text: NO_EVERYONE_PERMISSION.into() }).await?;
                                continue;
                            }
                        }
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
                        let edit_channel = state.lock().unwrap().db.message_channel_id(id)?.unwrap_or(0);
                        if let Some(refusal) = automod_refusal(&state, &user, edit_channel, &text, None)? {
                            if refusal.alert {
                                report_block(
                                    &state,
                                    &tx,
                                    &user,
                                    &display,
                                    edit_channel,
                                    &text,
                                    &refusal.reason,
                                    refusal.rule,
                                    &refusal.keyword,
                                )?;
                            }
                            if refusal.block {
                                let shown = if refusal.notice.is_empty() { refusal.reason } else { refusal.notice };
                                send(&mut writer, &ServerMsg::Error { text: shown }).await?;
                                continue;
                            }
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
                                // A message shown while the channel is open has been read.
                                state.lock().unwrap().db.mark_read(&user, channel_id)?;
                            }
                        } else {
                            // Tell the client there is new activity, with the message. A direct
                            // message is announced only to its members, and a channel only to members
                            // who may view it.
                            let allowed = {
                                let s = state.lock().unwrap();
                                let access = Access::new(&s.db, &user)?;
                                access.allows(message.channel_id, perm::VIEW_CHANNEL)
                                    && (!message.channel.starts_with("dm-")
                                        || s.db.is_dm_member(&message.channel, &user)?)
                            };
                            if allowed {
                                send(&mut writer, &ServerMsg::Activity { message }).await?;
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
                    Ok(ServerMsg::Typing { channel_id: c, user: who, name }) => {
                        // Not to the person typing, and only in the channel this connection has open.
                        if c == channel_id && who != user && Access::new(&state.lock().unwrap().db, &user)?.allows(c, perm::VIEW_CHANNEL) {
                            send(&mut writer, &ServerMsg::Typing { channel_id: c, user: who, name }).await?;
                        }
                    }
                    Ok(overrides @ ServerMsg::ChannelOverrides { .. }) => {
                        send(&mut writer, &overrides).await?;
                        // What each member can see may have changed, so their channel list is sent again.
                        send_visible_channels(&mut writer, &state, &user).await?;
                    }
                    Ok(overrides @ ServerMsg::CategoryOverrides { .. }) => {
                        send(&mut writer, &overrides).await?;
                    }
                    Ok(modes @ ServerMsg::SlowModes { .. }) => {
                        send(&mut writer, &modes).await?;
                    }
                    Ok(msg @ ServerMsg::MemberRenamed { .. }) => {
                        // This connection's own sends use the new name from now on.
                        if let ServerMsg::MemberRenamed { id, name } = &msg {
                            if *id == user {
                                display = name.clone();
                            }
                        }
                        send(&mut writer, &msg).await?;
                    }
                    Ok(overrides @ ServerMsg::MemberOverrides { .. }) => {
                        send(&mut writer, &overrides).await?;
                        // Which channels a member may see may have changed, so their channel list is sent again.
                        send_visible_channels(&mut writer, &state, &user).await?;
                    }
                    Ok(presence @ ServerMsg::Presence { .. }) => {
                        send(&mut writer, &presence).await?;
                    }
                    Ok(status @ ServerMsg::ProfileStatus { .. }) => {
                        send(&mut writer, &status).await?;
                    }
                    // Timeouts starting and ending are pushed to everyone, so the member timed out sees it at once.
                    Ok(timeout @ (ServerMsg::TimedOut { .. } | ServerMsg::TimeoutEnded { .. })) => {
                        send(&mut writer, &timeout).await?;
                    }
                    Ok(roles @ ServerMsg::Roles { .. }) => {
                        send(&mut writer, &roles).await?;
                        // Roles decide which channels a member can see, so their channel list is sent again.
                        send_visible_channels(&mut writer, &state, &user).await?;
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
                    Ok(ServerMsg::Removed { user: removed, banned, reason }) => {
                        // Only the member removed is told, and their connection ends.
                        if removed == user {
                            send(&mut writer, &ServerMsg::Removed { user: removed, banned, reason }).await?;
                            break;
                        }
                    }
                    Ok(ServerMsg::ServerDeleted) => {
                        send(&mut writer, &ServerMsg::ServerDeleted).await?;
                        break;
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

/// The direct message service: `VAXELIUM_DM`, or its default address.
fn dm_addr() -> String {
    std::env::var("VAXELIUM_DM").unwrap_or_else(|_| common::DEFAULT_DM_ADDR.to_string())
}

/// Passes a direct message request on to the direct message service and returns its reply. Its
/// certificate is checked against the one remembered, as the auth service's is. Used for the web
/// client, which cannot reach the service itself.
async fn dm_call(state: &SharedState, request: &common::DmRequest) -> Result<common::DmReply, String> {
    let addr = dm_addr();
    let (mut stream, fingerprint) = common::tls::connect_async(&addr)
        .await
        .map_err(|e| format!("direct message service unavailable: {e}"))?;
    if !pin_ok(state, "dm", &addr, &fingerprint) {
        return Err("the direct message service's certificate has changed".into());
    }
    let mut line = serde_json::to_string(request).map_err(|e| e.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("direct message service unavailable: {e}"))?;
    let mut lines = BufReader::new(stream).lines();
    let reply = lines
        .next_line()
        .await
        .map_err(|e| format!("direct message service unavailable: {e}"))?
        .ok_or_else(|| "direct message service closed the connection".to_string())?;
    serde_json::from_str(&reply).map_err(|e| format!("bad reply from direct message service: {e}"))
}

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
    let mut request = serde_json::to_string(&AuthRequest::Verify {
        token: token.to_string(),
    })
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

/// Sends a member the channels and categories they may view now. Sent again whenever permissions change,
/// so an open client shows a channel as soon as it becomes visible, and hides one that is no longer.
async fn send_visible_channels<W: AsyncWrite + Unpin>(
    writer: &mut W,
    state: &SharedState,
    user: &str,
) -> anyhow::Result<()> {
    let (channels, categories, uncategorized) = {
        let s = state.lock().unwrap();
        let access = Access::new(&s.db, user)?;
        let (categories, uncategorized) = visible_layout(&access, s.db.categories()?, s.db.uncategorized_channels()?);
        let channels =
            s.db.channel_list()?
                .into_iter()
                .filter(|c| access.allows(c.id, perm::VIEW_CHANNEL))
                .collect();
        (channels, categories, uncategorized)
    };
    send(writer, &ServerMsg::Channels { channels }).await?;
    send(
        writer,
        &ServerMsg::Categories {
            categories,
            uncategorized,
        },
    )
    .await?;
    Ok(())
}

async fn send<W: AsyncWrite + Unpin>(writer: &mut W, msg: &ServerMsg) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Joins a channel by its name for this account. Returns the channel's id and its history, or `None`
/// when the channel is a direct message the account is not a member of.
fn join_channel(
    state: &SharedState,
    user: &str,
    name: &str,
) -> Result<Option<(i64, Vec<ChatMessage>)>, rusqlite::Error> {
    // Direct messages are joined only by their two members. Other names starting with "dm-" are
    // refused, so a normal channel can never take one.
    let s = state.lock().unwrap();
    let id = if name.starts_with("dm-") {
        if !s.db.is_dm_member(name, user)? {
            return Ok(None);
        }
        s.db.ensure_channel(name)?
    } else {
        s.db.ensure_channel(name)?
    };
    let history = s.db.history(id)?;
    Ok(Some((id, history)))
}

const NO_CHANNEL_PERMISSION: &str = "you need the Manage Channels permission for that";
const NO_CATEGORY_LEAVE: &str = "you need Manage Channel in both categories to move a channel between them";

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
        return Ok(Power {
            owner: true,
            perms: perm::ALL,
            top: None,
        });
    }
    let mine = db.member_role_ids(user)?;
    // Everyone has @everyone, so its permissions apply first. It never counts as a member's highest role.
    let mut perms = order
        .iter()
        .find(|r| r.id == EVERYONE_ROLE)
        .map_or(0, |r| r.permissions);
    let mut top = None;
    for (index, role) in order.iter().enumerate() {
        if role.id != EVERYONE_ROLE && mine.contains(&role.id) {
            perms |= role.permissions;
            top.get_or_insert(index);
        }
    }
    Ok(Power {
        owner: false,
        perms,
        top,
    })
}

/// True when `user` has the permission. The owner has every permission.
/// Whether the member may do something in one channel, with the server-wide permission and the
/// channel's own settings for them.
fn may_in_channel(state: &SharedState, user: &str, channel: i64, permission: u32) -> rusqlite::Result<bool> {
    Ok(Access::new(&state.lock().unwrap().db, user)?.manages(channel, permission))
}

/// Whether the member may move a channel to the category `to` (None for no category). Moving within the
/// same category needs only the channel's own permission. Leaving a category, or joining one, needs Manage
/// channel in that category too.
fn may_change_category(state: &SharedState, user: &str, channel: i64, to: Option<i64>) -> rusqlite::Result<bool> {
    let from = state.lock().unwrap().db.channel_category(channel)?;
    if from == to {
        return Ok(true);
    }
    if let Some(from) = from {
        if !may_in_category(state, user, from, perm::MANAGE_CHANNELS)? {
            return Ok(false);
        }
    }
    if let Some(to) = to {
        if !may_in_category(state, user, to, perm::MANAGE_CHANNELS)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether the member may manage a category, with the server-wide permission and the category's own
/// settings for them.
fn may_in_category(state: &SharedState, user: &str, category: i64, permission: u32) -> rusqlite::Result<bool> {
    Ok(Access::new(&state.lock().unwrap().db, user)?.manages_category(category, permission))
}

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
    /// The member's account id.
    user: String,
    /// The roles the member holds, @everyone included.
    held: Vec<i64>,
    overrides: Vec<common::ChannelOverride>,
    /// The permissions set for members, for every member. Only this member's are used.
    member_overrides: Vec<common::MemberChannelOverride>,
    category_overrides: Vec<common::CategoryOverride>,
    member_category_overrides: Vec<common::MemberCategoryOverride>,
    /// The name of each channel, by id. Direct messages are told apart by their name.
    names: std::collections::HashMap<i64, String>,
}

impl Access {
    fn new(db: &Db, user: &str) -> rusqlite::Result<Self> {
        let order = db.role_list()?;
        let power = power_of(db, user, &order)?;
        let mut held = db.member_role_ids(user)?;
        held.push(EVERYONE_ROLE);
        Ok(Access {
            power,
            user: user.to_string(),
            held,
            overrides: db.channel_overrides()?,
            member_overrides: db.channel_member_overrides()?,
            category_overrides: db.category_overrides()?,
            member_category_overrides: db.category_member_overrides()?,
            names: db.channel_names()?,
        })
    }

    /// What the channel's own settings say about one permission for this member, `None` when they say nothing.
    fn channel_grant(&self, channel_id: i64, bit: u32) -> Option<bool> {
        let member = self
            .member_overrides
            .iter()
            .find(|o| o.channel_id == channel_id && o.member == self.user)
            .map(|o| o.setting());
        let roles: Vec<common::Setting> = self
            .overrides
            .iter()
            .filter(|o| o.channel_id == channel_id && self.held.contains(&o.role))
            .map(|o| o.setting())
            .collect();
        common::grant(member, &roles, bit)
    }

    /// What a category's settings say about one permission for this member, `None` when they say nothing.
    fn category_grant(&self, category_id: i64, bit: u32) -> Option<bool> {
        let member = self
            .member_category_overrides
            .iter()
            .find(|o| o.category_id == category_id && o.member == self.user)
            .map(|o| o.setting());
        let roles: Vec<common::Setting> = self
            .category_overrides
            .iter()
            .filter(|o| o.category_id == category_id && self.held.contains(&o.role))
            .map(|o| o.setting())
            .collect();
        common::grant(member, &roles, bit)
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
        self.channel_grant(channel_id, bit)
            .unwrap_or_else(|| self.power.has(bit))
    }

    /// Whether the member may manage a channel: rename, move, delete and change its permissions
    /// (`MANAGE_CHANNELS`), or delete other members' messages in it (`MANAGE_MESSAGES`). A channel's
    /// setting for them can give or take either one there. Direct messages can't be managed.
    fn manages(&self, channel_id: i64, bit: u32) -> bool {
        match self.names.get(&channel_id) {
            None => return false,
            Some(name) if name.starts_with("dm-") => return false,
            Some(_) => {}
        }
        self.channel_grant(channel_id, bit)
            .unwrap_or_else(|| self.power.has(bit))
    }

    /// Whether the member may manage a category: rename it, delete it, or change its permissions.
    fn manages_category(&self, category_id: i64, bit: u32) -> bool {
        self.category_grant(category_id, bit)
            .unwrap_or_else(|| self.power.has(bit))
    }
}

/// The categories and the channels outside them, with only the channels this member may view.
fn visible_layout(
    access: &Access,
    categories: Vec<common::CategoryInfo>,
    uncategorized: Vec<common::ChannelRef>,
) -> (Vec<common::CategoryInfo>, Vec<common::ChannelRef>) {
    // A category whose channels are all hidden from this member is left out, as it would show an empty heading.
    // A category that was empty to begin with is kept.
    let categories = categories
        .into_iter()
        .filter_map(|mut c| {
            let had_channels = !c.channels.is_empty();
            c.channels.retain(|ch| access.allows(ch.id, perm::VIEW_CHANNEL));
            (!had_channels || !c.channels.is_empty()).then_some(c)
        })
        .collect();
    let uncategorized = uncategorized
        .into_iter()
        .filter(|ch| access.allows(ch.id, perm::VIEW_CHANNEL))
        .collect();
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

/// The listed word `display` matches under AutoMod's name rule, or None when the name is fine or the
/// member is left alone: Administrator and Manage Server holders, and holders of an allowed role.
fn name_flag(state: &SharedState, user: &str, display: &str) -> rusqlite::Result<Option<String>> {
    let rules = state.lock().unwrap().db.automod()?.names;
    let Some(word) = rules.flags(display).map(str::to_string) else {
        return Ok(None);
    };
    if may(state, user, perm::MANAGE_SERVER)? {
        return Ok(None);
    }
    let held = state.lock().unwrap().db.member_role_ids(user)?;
    if held.iter().any(|r| rules.allowed_roles.contains(r)) {
        return Ok(None);
    }
    Ok(Some(word))
}

/// Why a member may not interact because of their name, or None when they may.
fn name_block(state: &SharedState, user: &str, display: &str) -> rusqlite::Result<Option<String>> {
    let rules = state.lock().unwrap().db.automod()?.names;
    if !rules.block_interactions || name_flag(state, user, display)?.is_none() {
        return Ok(None);
    }
    if !rules.notice.is_empty() {
        return Ok(Some(rules.notice));
    }
    Ok(Some(
        "AutoMod: your name is not allowed on this server. Change your display name to take part.".into(),
    ))
}

/// Posts AutoMod's alert about a flagged name to its channel, once per member and name, when the name
/// rule asks for alerts.
fn name_alert(
    state: &SharedState,
    tx: &tokio::sync::broadcast::Sender<ServerMsg>,
    user: &str,
    display: &str,
    word: &str,
) -> rusqlite::Result<()> {
    {
        let mut s = state.lock().unwrap();
        if !s.db.automod()?.names.send_alert || !s.name_alerts.insert((user.to_string(), display.to_string())) {
            return Ok(());
        }
    }
    let details = common::AutoModReport {
        user_id: user.to_string(),
        display: display.to_string(),
        channel_id: 0,
        channel: String::new(),
        rule: "Flagged Member Names".to_string(),
        keyword: word.to_string(),
        content: String::new(),
    };
    automod_post(
        state,
        tx,
        &format!("Flagged the name of **{display}**: it matches “{word}”."),
        Some(details),
    )
}

/// Reports to AutoMod's channel that it blocked `display` (`user`) in `channel`: the rule that caught it, the word
/// it matched, and what they tried to send. `reason` is the sentence for the report's text.
#[allow(clippy::too_many_arguments)]
fn report_block(
    state: &SharedState,
    tx: &tokio::sync::broadcast::Sender<ServerMsg>,
    user: &str,
    display: &str,
    channel: i64,
    text: &str,
    reason: &str,
    rule: &str,
    keyword: &str,
) -> rusqlite::Result<()> {
    let place = state.lock().unwrap().db.channel_name(channel)?.unwrap_or_default();
    // The reason as a sentence: "Your message uses ...", ending in a full stop.
    let why = reason.strip_prefix("AutoMod: ").unwrap_or(reason);
    let mut sentence = why.to_string();
    if let Some(first) = sentence.get(..1).map(str::to_uppercase) {
        sentence.replace_range(..1, &first);
    }
    if !sentence.ends_with(['.', '!', '?']) {
        sentence.push('.');
    }
    let mut report = format!("Blocked a message from **{display}** in #{place}\nReason: {sentence}");
    let quoted: Vec<String> = text.trim().lines().map(|l| format!("> {l}")).collect();
    if !quoted.is_empty() {
        report.push_str("\n\n");
        report.push_str(&quoted.join("\n"));
    }
    let details = common::AutoModReport {
        user_id: user.to_string(),
        display: display.to_string(),
        channel_id: channel,
        channel: place,
        rule: rule.to_string(),
        keyword: keyword.to_string(),
        content: text.trim().to_string(),
    };
    automod_post(state, tx, &report, Some(details))
}

/// Posts a message from AutoMod to its channel, when it has one.
fn automod_post(
    state: &SharedState,
    tx: &tokio::sync::broadcast::Sender<ServerMsg>,
    text: &str,
    report: Option<common::AutoModReport>,
) -> rusqlite::Result<()> {
    let message = {
        let s = state.lock().unwrap();
        let Some(channel_id) = s.db.automod()?.alert_channel else {
            return Ok(());
        };
        let Some(channel) = s.db.channel_name(channel_id)? else {
            return Ok(());
        };
        let mut message = ChatMessage {
            id: 0,
            channel_id,
            channel,
            author: "AutoMod".into(),
            author_id: common::AUTOMOD_ID.into(),
            text: text.to_string(),
            timestamp: now_secs(),
            edited: false,
            reactions: Vec::new(),
            reply: None,
            pinned: false,
            report,
            image: None,
        };
        message.id = s.db.save_message(&message)?;
        message
    };
    let _ = tx.send(ServerMsg::Message { message });
    Ok(())
}

const NO_AUTOMOD_PERMISSION: &str = "you need the Manage Server permission to change AutoMod";
const NO_SYSTEM_PERMISSION: &str = "you need the Manage Server permission to change system messages";

/// Why the server's automatic moderation refuses `text` from `user` in `channel`, or None when it may
/// go. Members who may manage messages there are not checked, nor are direct messages. When
/// `recent_sends` is given, this is a new message and the spam filter counts it.
/// Why AutoMod caught a message, and what it does about it.
struct Refusal {
    reason: String,
    /// What the member is shown, when it is not the reason. Empty means the reason.
    notice: String,
    /// Refuse the message. Off, it goes through and only the report and any timeout apply.
    block: bool,
    /// Post a report to AutoMod's channel.
    alert: bool,
    /// The rule that caught the message, as its report names it, and the word it matched (empty if none).
    rule: &'static str,
    keyword: String,
}

/// A refusal that blocks the message and is reported, as the word, link and spam rules are.
fn blocked(reason: String, rule: &'static str, keyword: String) -> Refusal {
    Refusal {
        reason,
        notice: String::new(),
        block: true,
        alert: true,
        rule,
        keyword,
    }
}

fn automod_refusal(
    state: &SharedState,
    user: &str,
    channel: i64,
    text: &str,
    recent_sends: Option<&mut std::collections::VecDeque<u64>>,
) -> rusqlite::Result<Option<Refusal>> {
    let (rules, in_dm) = {
        let s = state.lock().unwrap();
        let name = s.db.channel_name(channel)?.unwrap_or_default();
        (s.db.automod()?, name.starts_with("dm-"))
    };
    if in_dm || may_in_channel(state, user, channel, perm::MANAGE_MESSAGES)? {
        return Ok(None);
    }
    // A member AutoMod has timed out may not send until the time is up.
    let now = now_secs();
    let until = state.lock().unwrap().timeouts.get(user).copied();
    if let Some(until) = until.filter(|&until| until > now) {
        return Ok(Some(blocked(
            format!("AutoMod: you are timed out for {} more seconds", until - now),
            "Timeout",
            String::new(),
        )));
    }
    if let Some(breach) = rules.check(text) {
        return Ok(Some(blocked(breach.reason, breach.rule, breach.keyword)));
    }
    if let Some(flag) = rules.flagged.flags(text) {
        if exemptions_apply(state, user, channel, &rules.flagged.allowed_roles, &rules.flagged.allowed_channels)? {
            return Ok(Some(Refusal {
                reason: format!(
                    "AutoMod: your message uses a flagged word: “{}” ({}, {} severity)",
                    flag.word,
                    flag.category.name(),
                    format!("{:?}", flag.severity).to_lowercase()
                ),
                notice: rules.flagged.notice.clone(),
                block: rules.flagged.block,
                alert: rules.flagged.send_alert,
                rule: "AutoMod Flagged Words",
                keyword: flag.word.clone(),
            }));
        }
    }
    if let Some(matched) = rules.filtered.find(text) {
        if exemptions_apply(state, user, channel, &rules.filtered.allowed_roles, &rules.filtered.allowed_channels)? {
            return Ok(Some(Refusal {
                reason: format!("AutoMod: your message matches a filter: “{matched}”"),
                notice: rules.filtered.notice.clone(),
                block: rules.filtered.block,
                alert: rules.filtered.send_alert,
                rule: "AutoMod Filtered Patterns",
                keyword: matched,
            }));
        }
    }
    if let Some(word) = rules.words.find(text) {
        if exemptions_apply(state, user, channel, &rules.words.allowed_roles, &rules.words.allowed_channels)? {
            return Ok(Some(Refusal {
                reason: format!("AutoMod: your message uses a filtered word: “{word}”"),
                notice: rules.words.notice.clone(),
                block: rules.words.block,
                alert: rules.words.send_alert,
                rule: "AutoMod Filtered Words",
                keyword: word,
            }));
        }
    }
    if rules.mentions.enabled {
        if let Some(refusal) = mention_refusal(state, user, &rules.mentions, text, now)? {
            return Ok(Some(refusal));
        }
    }
    if let Some(sends) = recent_sends {
        while sends
            .front()
            .is_some_and(|&t| now.saturating_sub(t) >= common::SPAM_WINDOW_SECS)
        {
            sends.pop_front();
        }
        if rules.spam_filter && sends.len() >= common::SPAM_LIMIT {
            return Ok(Some(blocked(
                "AutoMod: you are sending messages too quickly".into(),
                "Spam Filter",
                String::new(),
            )));
        }
        sends.push_back(now);
    }
    Ok(None)
}

/// Whether the flagged words rule applies to `user` in `channel`. Members with Manage Server, holders of an
/// allowed role, and anyone in an allowed channel are left alone.
fn exemptions_apply(
    state: &SharedState,
    user: &str,
    channel: i64,
    allowed_roles: &[i64],
    allowed_channels: &[i64],
) -> rusqlite::Result<bool> {
    if allowed_channels.contains(&channel) || may(state, user, perm::MANAGE_SERVER)? {
        return Ok(false);
    }
    let held = state.lock().unwrap().db.member_role_ids(user)?;
    Ok(!held.iter().any(|r| allowed_roles.contains(r)))
}

/// What the mention rule does with `text`, or None when it does not apply. Counts the members and roles it
/// mentions, keeps the count for raid detection, and times the member out when the rule says so.
fn mention_refusal(
    state: &SharedState,
    user: &str,
    rule: &common::MentionRule,
    text: &str,
    now: u64,
) -> rusqlite::Result<Option<Refusal>> {
    // Members who may manage the server, members who may mention past the limit, and members with an allowed
    // role, are left alone.
    if may(state, user, perm::MANAGE_SERVER)? || may(state, user, perm::BYPASS_MENTION_LIMIT)? {
        return Ok(None);
    }
    let held = state.lock().unwrap().db.member_role_ids(user)?;
    if held.iter().any(|r| rule.allowed_roles.contains(r)) {
        return Ok(None);
    }
    let (members, roles) = {
        let s = state.lock().unwrap();
        let members: Vec<String> = s.db.display_names()?.into_values().collect();
        let roles: Vec<String> =
            s.db.role_list()?
                .into_iter()
                .filter(|r| r.mentionable && r.id != common::EVERYONE_ROLE)
                .map(|r| r.name)
                .collect();
        (members, roles)
    };
    let count = common::mentioned_count(text, &members, &roles);
    if count == 0 {
        return Ok(None);
    }
    let mut s = state.lock().unwrap();
    let log = s.mention_log.entry(user.to_string()).or_default();
    while log
        .front()
        .is_some_and(|&(at, _)| now.saturating_sub(at) >= common::RAID_WINDOW_SECS)
    {
        log.pop_front();
    }
    let recent: usize = log.iter().map(|&(_, n)| n).sum();
    log.push_back((now, count));
    let max = rule.max_unique as usize;
    let reason = if count > max {
        Some(format!(
            "AutoMod: your message mentions {count} people or roles, but the limit is {max}"
        ))
    } else if rule.raid_detection && recent + count >= common::RAID_MENTIONS {
        Some("AutoMod: too many people were mentioned in a short time".to_string())
    } else {
        None
    };
    let Some(reason) = reason else {
        return Ok(None);
    };
    if rule.timeout_secs > 0 {
        s.timeouts.insert(user.to_string(), now + rule.timeout_secs);
    }
    Ok(Some(Refusal {
        reason,
        notice: rule.notice.clone(),
        block: rule.block,
        alert: rule.send_alert,
        rule: "Mention Limit",
        keyword: String::new(),
    }))
}

/// Says the channel cannot be used by this member, for the message that refuses them.
const CANNOT_VIEW: &str = "you can't view that channel";
const CANNOT_SEND: &str = "you can't send messages in that channel";

/// The permissions @everyone keeps until another role has them.
const EVERYONE_KEEPS_BITS: u32 = perm::VIEW_CHANNEL | perm::SEND_MESSAGES;
const EVERYONE_KEEPS: &str = "@everyone keeps View Channels and Message Channels until another role has them";

/// True when setting @everyone's permissions to `permissions` would drop one of the kept bits that no
/// other role holds.
fn removes_everyone_keep(db: &Db, permissions: u32) -> rusqlite::Result<bool> {
    let others = db
        .role_list()?
        .into_iter()
        .filter(|r| r.id != EVERYONE_ROLE)
        .map(|r| r.permissions)
        .collect::<Vec<_>>();
    for bit in [perm::VIEW_CHANNEL, perm::SEND_MESSAGES] {
        if EVERYONE_KEEPS_BITS & bit != 0
            && permissions & bit == 0
            && !others.iter().any(|p| p & (bit | perm::ADMINISTRATOR) != 0)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A change to the roles, asked for by a member.
enum RoleChange {
    Create {
        name: String,
        color: String,
        permissions: u32,
        mentionable: bool,
        hoisted: bool,
    },
    Update {
        role: i64,
        name: String,
        color: String,
        permissions: u32,
        mentionable: bool,
        hoisted: bool,
    },
    Delete {
        role: i64,
    },
    Move {
        role: i64,
        before: Option<i64>,
    },
    Member {
        user: String,
        role: i64,
        on: bool,
    },
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
        return Ok(Err("you need the Manage Roles permission for that".into()));
    }
    let index_of = |id: i64| order.iter().position(|r| r.id == id);
    let below_you = "that role is not below your highest role";
    let no_role = "there is no such role";
    match change {
        RoleChange::Create {
            name,
            color,
            permissions,
            mentionable,
            hoisted,
        } => {
            let name = name.trim().to_string();
            if let Some(text) = role_fields_error(&power, &name, &color, permissions) {
                return Ok(Err(text));
            }
            match db.create_role(&name, &color, permissions, mentionable, hoisted)? {
                Some(_) => Ok(Ok(())),
                None => Ok(Err("a role with that name already exists".into())),
            }
        }
        RoleChange::Update {
            role,
            name,
            color,
            permissions,
            mentionable,
            hoisted,
        } => {
            // @everyone keeps its name.
            let name = if role == EVERYONE_ROLE {
                "@everyone".to_string()
            } else {
                name.trim().to_string()
            };
            // @everyone already reaches everyone, so it cannot be pinged or grouped.
            let (mentionable, hoisted) = if role == EVERYONE_ROLE {
                (false, false)
            } else {
                (mentionable, hoisted)
            };
            let Some(index) = index_of(role) else {
                return Ok(Err(no_role.into()));
            };
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
            // The AutoMod role is always an administrator, so the bot is not held to channel permissions.
            let permissions = if role == common::AUTOMOD_ROLE || role == common::SYSTEM_ROLE { perm::ADMINISTRATOR } else { permissions };
            db.update_role(role, &name, &color, permissions, mentionable, hoisted)?;
            Ok(Ok(()))
        }
        RoleChange::Delete { role } => {
            if role == EVERYONE_ROLE {
                return Ok(Err("@everyone cannot be deleted".into()));
            }
            if role == common::AUTOMOD_ROLE {
                return Ok(Err("the AutoMod role belongs to the bot and cannot be deleted".into()));
            }
            if role == common::SYSTEM_ROLE {
                return Ok(Err("the System role belongs to the server and cannot be deleted".into()));
            }
            let Some(index) = index_of(role) else {
                return Ok(Err(no_role.into()));
            };
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
            if role == common::AUTOMOD_ROLE {
                return Ok(Err("the AutoMod role stays where it is".into()));
            }
            if role == common::SYSTEM_ROLE {
                return Ok(Err("the System role stays where it is".into()));
            }
            let Some(index) = index_of(role) else {
                return Ok(Err(no_role.into()));
            };
            if !power.outranks(index) {
                return Ok(Err(below_you.into()));
            }
            // Only the real roles are reordered; @everyone and the AutoMod role keep their places.
            let mut ids: Vec<i64> = order
                .iter()
                .map(|r| r.id)
                .filter(|&id| id != EVERYONE_ROLE && id != common::AUTOMOD_ROLE && id != common::SYSTEM_ROLE)
                .collect();
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
            if role == common::AUTOMOD_ROLE {
                return Ok(Err("the AutoMod role belongs to the bot and cannot be given to members".into()));
            }
            if role == common::SYSTEM_ROLE {
                return Ok(Err("the System role belongs to the server and cannot be given to members".into()));
            }
            let Some(index) = index_of(role) else {
                return Ok(Err(no_role.into()));
            };
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
/// A change to the badges of this server, made by someone who may manage roles.
enum BadgeChange {
    Create { name: String, color: String },
    Delete { badge: i64 },
    Member { user: String, badge: i64, on: bool },
}

/// Applies a badge change, then tells every connection about the badges and the members' badges. Returns
/// the reason when the change is refused.
fn change_badges(
    state: &SharedState,
    tx: &broadcast::Sender<ServerMsg>,
    user: &str,
    change: BadgeChange,
) -> rusqlite::Result<Option<String>> {
    if !may(state, user, perm::MANAGE_ROLES)? {
        return Ok(Some("you need the Manage Roles permission to change badges".into()));
    }
    let refused = {
        let s = state.lock().unwrap();
        match change {
            BadgeChange::Create { name, color } => {
                let name = name.trim().to_string();
                let color = color.trim().to_string();
                if name.is_empty() || name.chars().count() > 32 {
                    Some("a badge needs a name of 1 to 32 characters".to_string())
                } else if color.len() > 16 {
                    Some("that colour is not valid".to_string())
                } else if s.db.create_badge(&name, &color)?.is_none() {
                    Some("a badge with that name already exists".to_string())
                } else {
                    None
                }
            }
            BadgeChange::Delete { badge } => match s.db.delete_badge(badge)? {
                true => None,
                false => Some("there is no such badge".to_string()),
            },
            BadgeChange::Member {
                user: member,
                badge,
                on,
            } => {
                if !s.db.badge_exists(badge)? {
                    Some("there is no such badge".to_string())
                } else if !s.db.is_member(&member)? {
                    Some("that person is not a member of this server".to_string())
                } else {
                    s.db.set_member_badge(&member, badge, on)?;
                    None
                }
            }
        }
    };
    if refused.is_some() {
        return Ok(refused);
    }
    let (badges, users) = {
        let s = state.lock().unwrap();
        (s.db.badge_list()?, s.presence())
    };
    let _ = tx.send(ServerMsg::Badges { badges });
    let _ = tx.send(ServerMsg::Presence { users });
    Ok(None)
}

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

/// Sends every member's permissions for channels and categories to everyone connected.
fn send_member_overrides(state: &Arc<Mutex<State>>, tx: &broadcast::Sender<ServerMsg>) -> Result<(), rusqlite::Error> {
    let (channels, categories) = {
        let s = state.lock().unwrap();
        (s.db.channel_member_overrides()?, s.db.category_member_overrides()?)
    };
    let _ = tx.send(ServerMsg::MemberOverrides { channels, categories });
    Ok(())
}

/// Sends the current categories to every connection.
fn broadcast_categories(state: &Arc<Mutex<State>>, tx: &broadcast::Sender<ServerMsg>) -> Result<(), rusqlite::Error> {
    let (categories, uncategorized) = {
        let s = state.lock().unwrap();
        (s.db.categories()?, s.db.uncategorized_channels()?)
    };
    let _ = tx.send(ServerMsg::Categories {
        categories,
        uncategorized,
    });
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

/// The longest reason given for a kick or ban, in characters.
const MAX_REMOVAL_REASON_LEN: usize = 200;
const NO_BAN_PERMISSION: &str = "you need the Ban Members permission for that";
const NO_KICK_PERMISSION: &str = "you need the Kick Members permission for that";
const NO_TIMEOUT_PERMISSION: &str = "you need the Timeout Members permission for that";
const NO_EVERYONE_PERMISSION: &str = "you need the Mention Everyone permission to use @everyone or @here";

/// The most messages in a channel that are counted as unread, so a long-unopened channel is not scanned forever.
const UNREAD_SCAN: i64 = 500;

/// What a member has not read in each channel they may view: the messages since they last read it, and how many
/// of those ping them (`@everyone`, `@here`, or a mentionable role they hold).
fn unread_counts(state: &SharedState, user: &str) -> rusqlite::Result<Vec<common::UnreadCount>> {
    let s = state.lock().unwrap();
    let access = Access::new(&s.db, user)?;
    let held_ids = s.db.member_role_ids(user)?;
    // What pings this member: the names of their mentionable roles, and their own name (`@name` mentions them).
    let mut held: Vec<String> = s
        .db
        .role_list()?
        .into_iter()
        .filter(|r| r.mentionable && r.id != common::EVERYONE_ROLE && held_ids.contains(&r.id))
        .map(|r| r.name)
        .collect();
    if let Some(own) = s.db.display_names()?.get(user) {
        held.push(own.clone());
    }
    let mut counts = Vec::new();
    for (channel_id, name) in s.db.channel_names()? {
        if name.starts_with("dm-") || !access.allows(channel_id, perm::VIEW_CHANNEL) {
            continue;
        }
        let Some(rows) = s.db.unread_messages(user, channel_id, UNREAD_SCAN)? else {
            continue;
        };
        if rows.is_empty() {
            continue;
        }
        let mentions = rows.iter().filter(|(_, text)| common::pings_viewer(text, &held)).count();
        counts.push(common::UnreadCount {
            channel_id,
            unread: rows.len() as u32,
            mentions: mentions as u32,
        });
    }
    Ok(counts)
}

/// The message a timed-out member sees when they try to send, with the time left rounded up to whole minutes.
fn timeout_notice(until: u64, now: u64) -> String {
    let minutes = until.saturating_sub(now).div_ceil(60).max(1);
    let unit = if minutes == 1 { "minute" } else { "minutes" };
    format!("you are timed out from sending messages for {minutes} more {unit}")
}

/// Times a member out from sending messages for `seconds`. The actor needs Timeout Members, the member's highest
/// role must be below theirs, and the owner cannot be timed out. Everyone is told.
fn timeout_member(
    state: &SharedState,
    tx: &broadcast::Sender<ServerMsg>,
    actor: &str,
    member: &str,
    seconds: u64,
    reason: &str,
) -> rusqlite::Result<Result<(), String>> {
    let s = state.lock().unwrap();
    let order = s.db.role_list()?;
    let power = power_of(&s.db, actor, &order)?;
    if !power.has(perm::TIMEOUT_MEMBERS) {
        return Ok(Err(NO_TIMEOUT_PERMISSION.into()));
    }
    if member == actor {
        return Ok(Err("you cannot time yourself out".into()));
    }
    if seconds == 0 || seconds > common::MAX_MEMBER_TIMEOUT_SECS {
        return Ok(Err(format!("a timeout must be between 1 second and {} days", common::MAX_MEMBER_TIMEOUT_SECS / 86_400)));
    }
    if !s.db.is_member(member)? {
        return Ok(Err("that person is not a member of this server".into()));
    }
    let target = power_of(&s.db, member, &order)?;
    if target.owner {
        return Ok(Err("the owner cannot be timed out".into()));
    }
    if !target.top.map_or(true, |top| power.outranks(top)) {
        return Ok(Err("that member's highest role is not below yours".into()));
    }
    let until = now_secs() + seconds;
    s.db.set_timeout(member, actor, reason, until)?;
    drop(s);
    let _ = tx.send(ServerMsg::TimedOut { user: member.to_string(), until, reason: reason.to_string() });
    arm_timeout_end(state, tx, member, until);
    Ok(Ok(()))
}

/// Tells everyone the moment a member's timeout ends. Does nothing if it was lifted or replaced in the meantime.
fn arm_timeout_end(state: &SharedState, tx: &broadcast::Sender<ServerMsg>, member: &str, until: u64) {
    let state = state.clone();
    let tx = tx.clone();
    let member = member.to_string();
    let wait = Duration::from_secs(until.saturating_sub(now_secs()));
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        let ended = state.lock().unwrap().db.end_timeout_at(&member, until).unwrap_or(false);
        if ended {
            let _ = tx.send(ServerMsg::TimeoutEnded { user: member });
        }
    });
}

/// A reason as typed: trimmed, and kept to a sensible length. Empty when none was given.
fn clean_reason(reason: Option<String>) -> String {
    reason
        .unwrap_or_default()
        .trim()
        .chars()
        .take(MAX_REMOVAL_REASON_LEN)
        .collect()
}

/// Removes a member from the server, and bans them too when `ban` is set. The actor needs the matching
/// permission, and the member's highest role must be below theirs. The owner cannot be removed. Everyone
/// is told; the member removed is told last, and their connection ends.
fn remove_member(
    state: &SharedState,
    tx: &broadcast::Sender<ServerMsg>,
    actor: &str,
    member: &str,
    reason: &str,
    ban: bool,
) -> rusqlite::Result<Result<(), String>> {
    let s = state.lock().unwrap();
    let order = s.db.role_list()?;
    let power = power_of(&s.db, actor, &order)?;
    let (needed, text) = if ban {
        (perm::BAN_MEMBERS, NO_BAN_PERMISSION)
    } else {
        (perm::KICK_MEMBERS, NO_KICK_PERMISSION)
    };
    if !power.has(needed) {
        return Ok(Err(text.into()));
    }
    if member == actor {
        return Ok(Err("you cannot remove yourself".into()));
    }
    if !s.db.is_member(member)? {
        return Ok(Err("that person is not a member of this server".into()));
    }
    let target = power_of(&s.db, member, &order)?;
    if target.owner {
        return Ok(Err("the owner cannot be kicked or banned".into()));
    }
    if !target.top.map_or(true, |top| power.outranks(top)) {
        return Ok(Err("that member's highest role is not below yours".into()));
    }
    if ban {
        s.db.ban(member, actor, reason, now_secs())?;
    }
    // A kick is leaving that the member is told about: they may join again unless they are banned.
    s.db.leave_server(member)?;
    if !ban {
        s.db.mark_kicked(member, reason)?;
    }
    let users = s.presence();
    drop(s);
    let _ = tx.send(ServerMsg::Presence { users });
    let _ = tx.send(ServerMsg::Removed {
        user: member.to_string(),
        banned: ban,
        reason: reason.to_string(),
    });
    Ok(Ok(()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "tests/main.rs"]
mod tests;
