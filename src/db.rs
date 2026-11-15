//! SQLite storage for channels and messages. Accounts live in the auth service.

use common::ChatMessage;
use rusqlite::{params, Connection};

/// Messages sent to a user when they join a channel.
pub const HISTORY_LEN: i64 = 200;
const DEFAULT_CHANNELS: [&str; 2] = ["general", "random"];
const DEFAULT_SERVER_NAME: &str = "My server";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS members (
    name       TEXT PRIMARY KEY,
    first_seen INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS server_info (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS categories (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    name     TEXT NOT NULL UNIQUE,
    position INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS channels (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL UNIQUE,
    category_id INTEGER,
    position    INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS reactions (
    message_id INTEGER NOT NULL,
    emoji      TEXT NOT NULL,
    user       TEXT NOT NULL,
    PRIMARY KEY (message_id, emoji, user)
);
CREATE TABLE IF NOT EXISTS session_cache (
    token    TEXT PRIMARY KEY,
    name     TEXT NOT NULL,
    verified INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS peers (
    addr TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS messages (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    channel   TEXT NOT NULL,
    author    TEXT NOT NULL,
    text      TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    edited    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS messages_by_channel ON messages (channel, id);
CREATE TABLE IF NOT EXISTS people (
    id      TEXT PRIMARY KEY,
    display TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS roles (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT NOT NULL UNIQUE,
    color       TEXT NOT NULL DEFAULT '',
    permissions INTEGER NOT NULL DEFAULT 0,
    position    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS member_roles (
    user_id TEXT NOT NULL,
    role_id INTEGER NOT NULL,
    PRIMARY KEY (user_id, role_id)
);
CREATE TABLE IF NOT EXISTS direct_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    a  TEXT NOT NULL,
    b  TEXT NOT NULL,
    UNIQUE (a, b)
);
CREATE TABLE IF NOT EXISTS user_status (
    user_id TEXT PRIMARY KEY,
    status  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS channel_overrides (
    channel_id INTEGER NOT NULL,
    role_id    INTEGER NOT NULL,
    view       INTEGER,
    send       INTEGER,
    PRIMARY KEY (channel_id, role_id)
);
";

pub struct Db {
    conn: Connection,
}

fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    if !table_exists(conn, table)? {
        return Ok(false);
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        params![table, column],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn self_id_missing(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM server_info WHERE key = 'server_id'", [], |row| row.get(0))?;
    Ok(count == 0)
}

/// True for an id that is not the random form new accounts get (32 lower-case hex characters).
pub fn is_legacy_id(id: &str) -> bool {
    id.is_empty() || id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Copies channels and categories from the tables made before they had IDs, keeping their names,
/// order and category. Runs only when those old tables are present.
fn migrate_to_ids(conn: &Connection) -> rusqlite::Result<()> {
    if table_exists(conn, "categories_v1")? {
        conn.execute("INSERT INTO categories (name, position) SELECT name, position FROM categories_v1 ORDER BY position", [])?;
        conn.execute("DROP TABLE categories_v1", [])?;
    }
    if table_exists(conn, "channels_v1")? {
        conn.execute("INSERT OR IGNORE INTO channels (name) SELECT name FROM channels_v1 ORDER BY name", [])?;
        if table_has_column(conn, "channels_v1", "category")? {
            conn.execute(
                "UPDATE channels SET category_id = (
                     SELECT cat.id FROM categories cat JOIN channels_v1 v ON v.category = cat.name
                     WHERE v.name = channels.name)",
                [],
            )?;
        }
        conn.execute("DROP TABLE channels_v1", [])?;
    }
    Ok(())
}

impl Db {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        // Channels and categories made before they had IDs are moved aside, rebuilt with IDs, and
        // their rows copied across below.
        for table in ["channels", "categories"] {
            if table_has_column(&conn, table, "id")? == false && table_exists(&conn, table)? {
                conn.execute(&format!("ALTER TABLE {table} RENAME TO {table}_v1"), [])?;
            }
        }
        conn.execute_batch(SCHEMA)?;
        // The @everyone role: held by every member, always at the bottom of the hierarchy.
        conn.execute(
            "INSERT OR IGNORE INTO roles (id, name, color, permissions, position) VALUES (0, '@everyone', '', 0, -1000000000)",
            [],
        )?;
        // Give @everyone View channels and Message channels once, for servers made before they existed.
        // The flag keeps a later change by the owner from being undone at the next start.
        if conn.execute("INSERT OR IGNORE INTO server_info (key, value) VALUES ('everyone_channel_perms', '1')", [])? == 1 {
            conn.execute(
                "UPDATE roles SET permissions = permissions | ?1 WHERE id = 0",
                params![(common::perm::VIEW_CHANNEL | common::perm::SEND_MESSAGES) as i64],
            )?;
        }
        migrate_to_ids(&conn)?;
        // Direct messages made before cross-server ones lack this column; they are all local.
        let _ = conn.execute("ALTER TABLE direct_messages ADD COLUMN cross INTEGER NOT NULL DEFAULT 0", []);
        // Databases made before messages could be edited lack this column.
        let _ = conn.execute("ALTER TABLE messages ADD COLUMN edited INTEGER NOT NULL DEFAULT 0", []);
        // Roles made before pings and member-list groups lack these columns.
        let _ = conn.execute("ALTER TABLE roles ADD COLUMN mentionable INTEGER NOT NULL DEFAULT 0", []);
        let _ = conn.execute("ALTER TABLE roles ADD COLUMN hoisted INTEGER NOT NULL DEFAULT 0", []);
        // Members get a display name separate from their id. Older databases lack it.
        let _ = conn.execute("ALTER TABLE members ADD COLUMN display TEXT", []);
        let _ = conn.execute("ALTER TABLE session_cache ADD COLUMN id TEXT", []);
        // Messages made before they named their channel by ID lack this column.
        let _ = conn.execute("ALTER TABLE messages ADD COLUMN channel_id INTEGER", []);
        // Channels made before they could be ordered lack this column; they keep their name order.
        let _ = conn.execute("ALTER TABLE channels ADD COLUMN position INTEGER NOT NULL DEFAULT 0", []);
        // The server's own ID, made once and kept. Clients recognise the server by it, so the server
        // stays the same when its address changes.
        if self_id_missing(&conn)? {
            conn.execute(
                "INSERT INTO server_info (key, value) VALUES ('server_id', ?1)",
                params![uuid::Uuid::new_v4().to_string()],
            )?;
        }
        // Accounts and users tables from older versions are no longer used. Chat servers keep no
        // accounts; the auth service does. A database with the auth service's `sessions` table is
        // never touched.
        let is_auth_db: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'sessions'",
            [],
            |row| row.get(0),
        )?;
        if is_auth_db == 0 {
            for table in ["accounts", "users"] {
                let found: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )?;
                if found > 0 {
                    conn.execute(&format!("DROP TABLE {table}"), [])?;
                    println!("removed the old {table} table from this database");
                }
            }
        }
        // Servers that already have members but no owner: the earliest member becomes the owner.
        conn.execute(
            "INSERT OR IGNORE INTO server_info (key, value)
             SELECT 'owner', name FROM members ORDER BY first_seen, rowid LIMIT 1",
            [],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO server_info (key, value) VALUES ('server_name', ?1)",
            params![DEFAULT_SERVER_NAME],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO server_info (key, value) VALUES ('server_description', '')",
            [],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO server_info (key, value) VALUES ('server_icon', '')",
            [],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO server_info (key, value) VALUES ('public', '0')",
            [],
        )?;
        // The default channels are made once, for a new database. A channel that was deleted later
        // stays deleted.
        let seeded: i64 = conn.query_row(
            "SELECT COUNT(*) FROM server_info WHERE key = 'channels_seeded'",
            [],
            |row| row.get(0),
        )?;
        if seeded == 0 {
            let existing: i64 = conn.query_row("SELECT COUNT(*) FROM channels", [], |row| row.get(0))?;
            if existing == 0 {
                for channel in DEFAULT_CHANNELS {
                    conn.execute("INSERT OR IGNORE INTO channels (name) VALUES (?1)", params![channel])?;
                }
            }
            conn.execute("INSERT INTO server_info (key, value) VALUES ('channels_seeded', '1')", [])?;
        }
        // Every message names its channel by ID. Channels with messages but no row (direct messages)
        // get one here.
        // Only messages that still lack a channel id may need a channel row. Channels deleted on
        // purpose must not come back from the names their old messages carry.
        conn.execute(
            "INSERT OR IGNORE INTO channels (name) SELECT DISTINCT channel FROM messages WHERE channel_id IS NULL",
            [],
        )?;
        conn.execute(
            "UPDATE messages SET channel_id = (SELECT id FROM channels WHERE channels.name = messages.channel)
             WHERE channel_id IS NULL",
            [],
        )?;
        Ok(Self { conn })
    }

    /// Remembers that this account has been on the server, and its name for now. Later logins don't
    /// add it again. The first member ever added becomes the owner. The owner never changes after
    /// that. `id` is the account's hidden id.
    pub fn add_member(&self, id: &str, display: &str, now: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO members (name, first_seen) VALUES (?1, ?2)",
            params![id, now as i64],
        )?;
        self.conn.execute(
            "UPDATE members SET display = ?2 WHERE name = ?1",
            params![id, display],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO server_info (key, value) VALUES ('owner', ?1)",
            params![id],
        )?;
        Ok(())
    }

    /// When each member first joined this server, by id, in Unix seconds.
    pub fn first_seen_all(&self) -> rusqlite::Result<std::collections::HashMap<String, u64>> {
        let mut stmt = self.conn.prepare("SELECT name, first_seen FROM members")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?.max(0) as u64)))?;
        rows.collect()
    }

    /// The name to show for each member, by id.
    pub fn display_names(&self) -> rusqlite::Result<std::collections::HashMap<String, String>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, COALESCE(display, name) FROM members
             UNION
             SELECT id, display FROM people WHERE id NOT IN (SELECT name FROM members)",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        rows.collect()
    }

    /// The owner of this server, by id, if anyone has joined it yet.
    /// Remembers the display name of someone who is not a member of this server, such as the
    /// sender of a direct message relayed from another server.
    pub fn add_person(&self, id: &str, display: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO people (id, display) VALUES (?1, ?2)
             ON CONFLICT (id) DO UPDATE SET display = excluded.display",
            params![id, display],
        )?;
        Ok(())
    }

    /// Whether this server has a member with this account id.
    pub fn is_member(&self, id: &str) -> rusqlite::Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM members WHERE name = ?1",
            params![id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// The two members of a direct message channel, if it is one.
    pub fn direct_message_pair(&self, channel: &str) -> rusqlite::Result<Option<(String, String)>> {
        use rusqlite::OptionalExtension;
        let Some(id) = channel.strip_prefix("dm-").and_then(|n| n.parse::<i64>().ok()) else {
            return Ok(None);
        };
        self.conn
            .query_row("SELECT a, b FROM direct_messages WHERE id = ?1", params![id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()
    }

    /// Whether a direct message has a member who is not on this server. Such a conversation is
    /// kept in step with the other server, so it cannot be edited, deleted or reacted to.
    pub fn is_cross_server_dm(&self, channel: &str) -> rusqlite::Result<bool> {
        let Some(id) = channel.strip_prefix("dm-").and_then(|n| n.parse::<i64>().ok()) else {
            return Ok(false);
        };
        let cross: i64 = self.conn.query_row(
            "SELECT cross FROM direct_messages WHERE id = ?1",
            params![id],
            |row| row.get(0),
        ).unwrap_or(0);
        Ok(cross != 0)
    }

    /// The direct message channel between two members, created the first time. A conversation
    /// with someone who is not a member of this server is marked as cross-server.
    pub fn open_direct_message(&self, a: &str, b: &str) -> rusqlite::Result<String> {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let cross = !(self.is_member(lo)? && self.is_member(hi)?);
        self.conn.execute(
            "INSERT OR IGNORE INTO direct_messages (a, b, cross) VALUES (?1, ?2, ?3)",
            params![lo, hi, cross as i64],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM direct_messages WHERE a = ?1 AND b = ?2",
            params![lo, hi],
            |row| row.get(0),
        )?;
        Ok(format!("dm-{id}"))
    }

    /// Whether `user` is one of the two members of a direct message channel.
    pub fn is_direct_message_member(&self, channel: &str, user: &str) -> rusqlite::Result<bool> {
        let Some(id) = channel.strip_prefix("dm-").and_then(|n| n.parse::<i64>().ok()) else {
            return Ok(false);
        };
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM direct_messages WHERE id = ?1 AND (a = ?2 OR b = ?2)",
            params![id, user],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// The direct message channels `user` is part of, each with the other member's id and when
    /// its last message was sent (0 if none). The most recent conversation comes first.
    pub fn direct_messages_of(&self, user: &str) -> rusqlite::Result<Vec<(String, String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.id, d.a, d.b, COALESCE(MAX(m.timestamp), 0) AS last
             FROM direct_messages d
             LEFT JOIN messages m ON m.channel = 'dm-' || d.id
             WHERE d.a = ?1 OR d.b = ?1
             GROUP BY d.id
             ORDER BY last DESC, d.id DESC",
        )?;
        let rows = stmt.query_map(params![user], |row| {
            let id: i64 = row.get(0)?;
            let a: String = row.get(1)?;
            let b: String = row.get(2)?;
            let last: i64 = row.get(3)?;
            let other = if a == user { b } else { a };
            Ok((format!("dm-{id}"), other, last as u64))
        })?;
        rows.collect()
    }

    /// The member whose display name is `display`, if there is one.
    pub fn member_by_display_name(&self, display: &str) -> rusqlite::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        self.conn
            .query_row(
                "SELECT name FROM members WHERE COALESCE(display, name) = ?1 LIMIT 1",
                params![display],
                |row| row.get(0),
            )
            .optional()
    }

    /// The channel a message is in, if the message exists.
    pub fn message_channel(&self, id: i64) -> rusqlite::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        self.conn
            .query_row("SELECT channel FROM messages WHERE id = ?1", params![id], |row| row.get(0))
            .optional()
    }

    pub fn owner(&self) -> rusqlite::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        self.conn
            .query_row("SELECT value FROM server_info WHERE key = 'owner'", [], |row| row.get(0))
            .optional()
    }

    /// The name everyone sees for this server.
    pub fn server_name(&self) -> rusqlite::Result<String> {
        self.conn.query_row(
            "SELECT value FROM server_info WHERE key = 'server_name'",
            [],
            |row| row.get(0),
        )
    }

    pub fn server_description(&self) -> rusqlite::Result<String> {
        self.conn.query_row(
            "SELECT value FROM server_info WHERE key = 'server_description'",
            [],
            |row| row.get(0),
        )
    }

    /// The server icon as base64 PNG, or `None` when the server has no icon.
    pub fn server_icon(&self) -> rusqlite::Result<Option<String>> {
        let icon: String = self.conn.query_row(
            "SELECT value FROM server_info WHERE key = 'server_icon'",
            [],
            |row| row.get(0),
        )?;
        Ok(if icon.is_empty() { None } else { Some(icon) })
    }

    pub fn set_server_icon(&self, png_base64: Option<&str>) -> rusqlite::Result<()> {
        self.set_server_info("server_icon", png_base64.unwrap_or(""))
    }

    pub fn set_server_name(&self, name: &str) -> rusqlite::Result<()> {
        self.set_server_info("server_name", name)
    }

    pub fn set_server_description(&self, description: &str) -> rusqlite::Result<()> {
        self.set_server_info("server_description", description)
    }

    /// A value from the server_info table, if it is set.
    pub fn info_value(&self, key: &str) -> rusqlite::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        self.conn
            .query_row("SELECT value FROM server_info WHERE key = ?1", params![key], |row| row.get(0))
            .optional()
    }

    /// Forgets the auth service certificates this server remembers. Returns how many were removed.
    pub fn forget_auth_pins(&self) -> rusqlite::Result<usize> {
        self.conn.execute("DELETE FROM server_info WHERE key LIKE 'pin:auth:%'", [])
    }

    pub fn set_server_info(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO server_info (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// True once another server has got in touch with this one, so other servers know it.
    pub fn is_public(&self) -> rusqlite::Result<bool> {
        let value: String = self.conn.query_row(
            "SELECT value FROM server_info WHERE key = 'public'",
            [],
            |row| row.get(0),
        )?;
        Ok(value == "1")
    }

    pub fn mark_public(&self) -> rusqlite::Result<()> {
        self.set_server_info("public", "1")
    }

    /// Addresses of other servers this one knows about.
    pub fn peers(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT addr FROM peers ORDER BY addr")?;
        let addrs = stmt.query_map([], |row| row.get(0))?;
        addrs.collect()
    }

    pub fn add_peer(&self, addr: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("INSERT OR IGNORE INTO peers (addr) VALUES (?1)", params![addr])?;
        Ok(())
    }

    pub fn remove_peer(&self, addr: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM peers WHERE addr = ?1", params![addr])?;
        Ok(())
    }

    /// Remembers that the auth service confirmed this token at time `verified`.
    pub fn cache_session(&self, token: &str, id: &str, name: &str, verified: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO session_cache (token, id, name, verified) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(token) DO UPDATE SET id = excluded.id, name = excluded.name, verified = excluded.verified",
            params![token, id, name, verified as i64],
        )?;
        Ok(())
    }

    /// The name and last confirmed time for a token that was verified before, if any.
    /// The account id, name and last confirmed time for a token that was verified before, if any.
    pub fn cached_session(&self, token: &str) -> rusqlite::Result<Option<(String, String, u64)>> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT COALESCE(id, name), name, verified FROM session_cache WHERE token = ?1",
                params![token],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)? as u64)),
            )
            .optional()?;
        Ok(row)
    }

    pub fn forget_cached_session(&self, token: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM session_cache WHERE token = ?1", params![token])?;
        Ok(())
    }

    /// Everyone who has been a member, including anyone who has posted a message.
    pub fn member_names(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT name FROM members UNION SELECT author FROM messages ORDER BY 1",
        )?;
        let names = stmt.query_map([], |row| row.get(0))?;
        names.collect()
    }

    /// The channel with this name, created if it does not exist. Returns its id.
    pub fn ensure_channel(&self, name: &str) -> rusqlite::Result<i64> {
        // A new channel goes last in the list.
        self.conn.execute(
            "INSERT OR IGNORE INTO channels (name, position)
             VALUES (?1, (SELECT COALESCE(MAX(position), 0) + 1 FROM channels))",
            params![name],
        )?;
        self.channel_id(name)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
    }

    /// The id of the channel with this name, if there is one.
    pub fn channel_id(&self, name: &str) -> rusqlite::Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        self.conn.query_row("SELECT id FROM channels WHERE name = ?1", params![name], |row| row.get(0)).optional()
    }

    /// The current name of a channel, if it exists.
    pub fn channel_name(&self, id: i64) -> rusqlite::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        self.conn.query_row("SELECT name FROM channels WHERE id = ?1", params![id], |row| row.get(0)).optional()
    }

    /// Every ordinary channel, by name. Direct messages have channel rows too, but they are not
    /// listed with the channels.
    pub fn channel_list(&self) -> rusqlite::Result<Vec<common::ChannelRef>> {
        let mut stmt = self.conn.prepare("SELECT id, name FROM channels WHERE name NOT LIKE 'dm-%' ORDER BY position, name")?;
        let rows = stmt.query_map([], |row| Ok(common::ChannelRef { id: row.get(0)?, name: row.get(1)? }))?;
        rows.collect()
    }

    /// Adds a category. Returns its id, or `None` when one with that name already exists.
    pub fn create_category(&self, name: &str) -> rusqlite::Result<Option<i64>> {
        let position: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM categories",
            [],
            |row| row.get(0),
        )?;
        let added = self
            .conn
            .execute("INSERT OR IGNORE INTO categories (name, position) VALUES (?1, ?2)", params![name, position])?;
        if added == 0 {
            return Ok(None);
        }
        Ok(Some(self.conn.last_insert_rowid()))
    }

    /// Removes a category. Its channels are kept and become uncategorized. Returns false when there
    /// was no such category.
    pub fn delete_category(&self, id: i64) -> rusqlite::Result<bool> {
        self.conn.execute("UPDATE channels SET category_id = NULL WHERE category_id = ?1", params![id])?;
        let removed = self.conn.execute("DELETE FROM categories WHERE id = ?1", params![id])?;
        Ok(removed > 0)
    }

    pub fn category_exists(&self, id: i64) -> rusqlite::Result<bool> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM categories WHERE id = ?1", params![id], |row| row.get(0))?;
        Ok(count > 0)
    }

    /// Puts a channel in a category, or takes it out with `None`.
    pub fn move_channel(&self, channel: i64, category: Option<i64>) -> rusqlite::Result<()> {
        // Moved channels go to the end of their new group.
        self.conn.execute(
            "UPDATE channels SET category_id = ?2,
                 position = (SELECT COALESCE(MAX(position), 0) + 1 FROM channels)
             WHERE id = ?1",
            params![channel, category],
        )?;
        Ok(())
    }

    /// Puts a channel in a category (or none) before another channel of that group, or at the end
    /// when `before` is None or not in the group. Renumbers the group. Returns false when the channel
    /// does not exist.
    pub fn place_channel(&self, channel: i64, category: Option<i64>, before: Option<i64>) -> rusqlite::Result<bool> {
        use rusqlite::OptionalExtension;
        let exists: Option<i64> = self
            .conn
            .query_row("SELECT id FROM channels WHERE id = ?1", params![channel], |row| row.get(0))
            .optional()?;
        if exists.is_none() {
            return Ok(false);
        }
        self.conn.execute("UPDATE channels SET category_id = ?2 WHERE id = ?1", params![channel, category])?;
        let mut stmt = self.conn.prepare(
            "SELECT id FROM channels WHERE category_id IS ?1 AND id != ?2 ORDER BY position, name",
        )?;
        let mut order: Vec<i64> = stmt
            .query_map(params![category, channel], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let at = before.and_then(|b| order.iter().position(|&c| c == b)).unwrap_or(order.len());
        order.insert(at, channel);
        for (i, id) in order.iter().enumerate() {
            self.conn.execute("UPDATE channels SET position = ?2 WHERE id = ?1", params![id, i as i64 + 1])?;
        }
        Ok(true)
    }

    /// The channels that are in no category, in order.
    pub fn uncategorized_channels(&self) -> rusqlite::Result<Vec<common::ChannelRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name FROM channels WHERE category_id IS NULL AND name NOT LIKE 'dm-%' ORDER BY position, name",
        )?;
        let rows = stmt.query_map([], |row| Ok(common::ChannelRef { id: row.get(0)?, name: row.get(1)? }))?;
        rows.collect()
    }

    /// Deletes a channel with its messages and their reactions. Returns false when there is no such
    /// channel.
    pub fn delete_channel(&self, id: i64) -> rusqlite::Result<bool> {
        // Messages are matched by id, and by name for messages written before ids existed, so none
        // are left behind to bring the channel back.
        let name = self.channel_name(id)?.unwrap_or_default();
        let matches = "channel_id = ?1 OR (channel_id IS NULL AND channel = ?2)";
        self.conn.execute(
            &format!("DELETE FROM reactions WHERE message_id IN (SELECT id FROM messages WHERE {matches})"),
            params![id, name],
        )?;
        self.conn.execute(&format!("DELETE FROM messages WHERE {matches}"), params![id, name])?;
        Ok(self.conn.execute("DELETE FROM channels WHERE id = ?1", params![id])? > 0)
    }

    /// Moves a category so it comes before `before` (or last, when `None`), and renumbers the order.
    /// Returns false when the category or the target does not exist.
    pub fn move_category(&self, id: i64, before: Option<i64>) -> rusqlite::Result<bool> {
        let mut stmt = self.conn.prepare("SELECT id FROM categories ORDER BY position, name")?;
        let mut order: Vec<i64> = stmt.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
        if !order.contains(&id) || before.is_some_and(|b| !order.contains(&b) || b == id) {
            return Ok(false);
        }
        order.retain(|&c| c != id);
        let at = match before {
            Some(b) => order.iter().position(|&c| c == b).unwrap_or(order.len()),
            None => order.len(),
        };
        order.insert(at, id);
        for (position, category) in order.iter().enumerate() {
            self.conn.execute(
                "UPDATE categories SET position = ?2 WHERE id = ?1",
                params![category, position as i64 + 1],
            )?;
        }
        Ok(true)
    }

    /// Ids stored on this server that are still the old form (a name, not a random id). Each is
    /// one that the auth service may have given a random id.
    pub fn legacy_ids(&self) -> rusqlite::Result<Vec<String>> {
        let sources = [
            "SELECT name FROM members",
            "SELECT author FROM messages",
            "SELECT user FROM reactions",
            "SELECT a FROM direct_messages",
            "SELECT b FROM direct_messages",
            "SELECT id FROM people",
            "SELECT value FROM server_info WHERE key = 'owner'",
            "SELECT id FROM session_cache WHERE id IS NOT NULL",
        ];
        let mut ids = std::collections::BTreeSet::new();
        for sql in sources {
            let mut stmt = self.conn.prepare(sql)?;
            for id in stmt.query_map([], |row| row.get::<_, String>(0))? {
                let id = id?;
                if is_legacy_id(&id) {
                    ids.insert(id);
                }
            }
        }
        Ok(ids.into_iter().collect())
    }

    /// Replaces old ids with the new ones the auth service gave them, everywhere this server stored
    /// them. Returns how many references were changed.
    pub fn apply_id_map(&self, pairs: &[(String, String)]) -> rusqlite::Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut changed = 0;
        for (old, new) in pairs {
            changed += tx.execute("UPDATE OR IGNORE members SET name = ?2 WHERE name = ?1", params![old, new])?;
            changed += tx.execute("UPDATE messages SET author = ?2 WHERE author = ?1", params![old, new])?;
            changed += tx.execute("UPDATE reactions SET user = ?2 WHERE user = ?1", params![old, new])?;
            changed += tx.execute("UPDATE OR IGNORE people SET id = ?2 WHERE id = ?1", params![old, new])?;
            changed += tx.execute("UPDATE server_info SET value = ?2 WHERE key = 'owner' AND value = ?1", params![old, new])?;
            changed += tx.execute("UPDATE OR IGNORE session_cache SET id = ?2 WHERE id = ?1", params![old, new])?;
            // A direct message is stored with its two members in order, so the pair is put back in order.
            let rows: Vec<(i64, String, String)> = {
                let mut stmt = tx.prepare("SELECT id, a, b FROM direct_messages WHERE a = ?1 OR b = ?1")?;
                let found = stmt.query_map(params![old], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
                found.collect::<rusqlite::Result<_>>()?
            };
            for (id, a, b) in rows {
                let a = if a == *old { new.clone() } else { a };
                let b = if b == *old { new.clone() } else { b };
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                changed += tx.execute("UPDATE OR IGNORE direct_messages SET a = ?2, b = ?3 WHERE id = ?1", params![id, lo, hi])?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Renames a channel. Returns false when the new name is taken or the channel does not exist.
    /// Messages refer to the channel by id, so they need no change.
    pub fn rename_channel(&self, id: i64, new: &str) -> rusqlite::Result<bool> {
        let taken: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM channels WHERE name = ?1 AND id != ?2",
            params![new, id],
            |row| row.get(0),
        )?;
        if taken > 0 {
            return Ok(false);
        }
        let changed = self.conn.execute("UPDATE channels SET name = ?2 WHERE id = ?1", params![id, new])?;
        Ok(changed > 0)
    }

    /// Every category in order, with its channels.
    pub fn categories(&self) -> rusqlite::Result<Vec<common::CategoryInfo>> {
        let mut stmt = self.conn.prepare("SELECT id, name FROM categories ORDER BY position, name")?;
        let cats: Vec<(i64, String)> = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for (id, name) in cats {
            let mut stmt = self.conn.prepare("SELECT id, name FROM channels WHERE category_id = ?1 ORDER BY position, name")?;
            let channels = stmt
                .query_map(params![id], |row| Ok(common::ChannelRef { id: row.get(0)?, name: row.get(1)? }))?
                .collect::<rusqlite::Result<_>>()?;
            out.push(common::CategoryInfo { id, name, channels });
        }
        Ok(out)
    }

    /// Saves a message and returns its new id.
    /// Every role, highest first.
    /// How each member has set their status, by account id. Members who never set one are not listed.
    pub fn statuses(&self) -> rusqlite::Result<std::collections::HashMap<String, common::PresenceStatus>> {
        let mut stmt = self.conn.prepare("SELECT user_id, status FROM user_status")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        rows.map(|r| r.map(|(id, key)| (id, common::PresenceStatus::from_key(&key)))).collect()
    }

    pub fn set_status(&self, user: &str, status: common::PresenceStatus) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO user_status (user_id, status) VALUES (?1, ?2)",
            params![user, status.key()],
        )?;
        Ok(())
    }

    /// The name of every channel, by id.
    pub fn channel_names(&self) -> rusqlite::Result<std::collections::HashMap<i64, String>> {
        let mut stmt = self.conn.prepare("SELECT id, name FROM channels")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
        rows.collect()
    }

    /// Every channel's exceptions for roles.
    pub fn channel_overrides(&self) -> rusqlite::Result<Vec<common::ChannelOverride>> {
        let mut stmt = self.conn.prepare("SELECT channel_id, role_id, view, send FROM channel_overrides ORDER BY channel_id, role_id")?;
        let rows = stmt.query_map([], |row| {
            Ok(common::ChannelOverride {
                channel_id: row.get(0)?,
                role: row.get(1)?,
                view: row.get::<_, Option<i64>>(2)?.map(|v| v != 0),
                send: row.get::<_, Option<i64>>(3)?.map(|v| v != 0),
            })
        })?;
        rows.collect()
    }

    /// Sets a role's exception for a channel. Both `None` removes it.
    pub fn set_channel_override(&self, channel_id: i64, role: i64, view: Option<bool>, send: Option<bool>) -> rusqlite::Result<()> {
        if view.is_none() && send.is_none() {
            self.conn.execute("DELETE FROM channel_overrides WHERE channel_id = ?1 AND role_id = ?2", params![channel_id, role])?;
        } else {
            self.conn.execute(
                "INSERT OR REPLACE INTO channel_overrides (channel_id, role_id, view, send) VALUES (?1, ?2, ?3, ?4)",
                params![channel_id, role, view.map(i64::from), send.map(i64::from)],
            )?;
        }
        Ok(())
    }

    pub fn role_list(&self) -> rusqlite::Result<Vec<common::RoleInfo>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, color, permissions, position, mentionable, hoisted FROM roles ORDER BY position DESC, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(common::RoleInfo {
                id: row.get(0)?,
                name: row.get(1)?,
                color: row.get(2)?,
                permissions: row.get::<_, i64>(3)? as u32,
                position: row.get(4)?,
                mentionable: row.get::<_, i64>(5)? != 0,
                hoisted: row.get::<_, i64>(6)? != 0,
            })
        })?;
        rows.collect()
    }

    /// Adds a role at the bottom of the hierarchy. Returns its id, or `None` when the name is taken.
    pub fn create_role(&self, name: &str, color: &str, permissions: u32, mentionable: bool, hoisted: bool) -> rusqlite::Result<Option<i64>> {
        // Above @everyone, which sits at the very bottom.
        let position: i64 = self.conn.query_row(
            "SELECT COALESCE(MIN(position), 1) - 1 FROM roles WHERE id != 0",
            [],
            |row| row.get(0),
        )?;
        let added = self.conn.execute(
            "INSERT OR IGNORE INTO roles (name, color, permissions, position, mentionable, hoisted) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![name, color, permissions as i64, position, mentionable as i64, hoisted as i64],
        )?;
        Ok((added > 0).then(|| self.conn.last_insert_rowid()))
    }

    /// True when another role (not `except`) already has this name.
    pub fn role_name_taken(&self, name: &str, except: i64) -> rusqlite::Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM roles WHERE name = ?1 AND id != ?2",
            params![name, except],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Changes a role's name, colour and permissions. Returns false when there is no such role.
    pub fn update_role(&self, id: i64, name: &str, color: &str, permissions: u32, mentionable: bool, hoisted: bool) -> rusqlite::Result<bool> {
        let changed = self.conn.execute(
            "UPDATE roles SET name = ?2, color = ?3, permissions = ?4, mentionable = ?5, hoisted = ?6 WHERE id = ?1",
            params![id, name, color, permissions as i64, mentionable as i64, hoisted as i64],
        )?;
        Ok(changed > 0)
    }

    /// Removes a role and takes it from everyone who had it. Returns false when there is no such role.
    pub fn delete_role(&self, id: i64) -> rusqlite::Result<bool> {
        self.conn.execute("DELETE FROM member_roles WHERE role_id = ?1", params![id])?;
        Ok(self.conn.execute("DELETE FROM roles WHERE id = ?1", params![id])? > 0)
    }

    /// Sets the order of the roles, highest first. Every role must be in `order`.
    pub fn set_role_order(&self, order: &[i64]) -> rusqlite::Result<()> {
        for (index, id) in order.iter().enumerate() {
            self.conn.execute(
                "UPDATE roles SET position = ?2 WHERE id = ?1",
                params![id, (order.len() - index) as i64],
            )?;
        }
        Ok(())
    }

    /// Gives a member a role, or takes it away.
    pub fn set_member_role(&self, user: &str, role: i64, on: bool) -> rusqlite::Result<()> {
        if on {
            self.conn.execute(
                "INSERT OR IGNORE INTO member_roles (user_id, role_id) VALUES (?1, ?2)",
                params![user, role],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM member_roles WHERE user_id = ?1 AND role_id = ?2",
                params![user, role],
            )?;
        }
        Ok(())
    }

    /// The ids of the roles a member has.
    pub fn member_role_ids(&self, user: &str) -> rusqlite::Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT role_id FROM member_roles WHERE user_id = ?1")?;
        let ids = stmt.query_map(params![user], |row| row.get(0))?;
        ids.collect()
    }

    /// The role ids of every member who has any role, by member id.
    pub fn all_member_roles(&self) -> rusqlite::Result<std::collections::HashMap<String, Vec<i64>>> {
        let mut stmt = self.conn.prepare("SELECT user_id, role_id FROM member_roles")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
        let mut map: std::collections::HashMap<String, Vec<i64>> = std::collections::HashMap::new();
        for row in rows {
            let (user, role) = row?;
            map.entry(user).or_default().push(role);
        }
        Ok(map)
    }

    pub fn save_message(&self, m: &ChatMessage) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO messages (channel, channel_id, author, text, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
            // Messages are stored under the author's id, so they stay with the account.
            params![m.channel, m.channel_id, m.author_id, m.text, m.timestamp as i64],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Deletes a message if `requester` wrote it, or always when `any` is true (the server owner,
    /// except in direct messages, which only their members can delete from).
    /// Returns the message's channel id, or `None` if nothing was deleted.
    pub fn delete_message(&self, id: i64, requester: &str, any: bool) -> rusqlite::Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        let channel: Option<i64> = self
            .conn
            .query_row(
                "SELECT m.channel_id FROM messages m JOIN channels c ON c.id = m.channel_id
                 WHERE m.id = ?1 AND (m.author = ?3 OR (?2 AND c.name NOT LIKE 'dm-%'))",
                params![id, any, requester],
                |row| row.get(0),
            )
            .optional()?;
        if channel.is_some() {
            self.conn.execute("DELETE FROM reactions WHERE message_id = ?1", params![id])?;
            self.conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        }
        Ok(channel)
    }

    /// Adds `user`'s reaction to a message, or removes it if they already reacted with that emoji.
    /// Returns the message's channel id and its reactions afterwards, or `None` if there is no such
    /// message.
    pub fn toggle_reaction(
        &self,
        id: i64,
        emoji: &str,
        user: &str,
    ) -> rusqlite::Result<Option<(i64, Vec<common::Reaction>)>> {
        use rusqlite::OptionalExtension;
        let channel: Option<i64> = self
            .conn
            .query_row("SELECT channel_id FROM messages WHERE id = ?1", params![id], |row| row.get(0))
            .optional()?;
        let Some(channel) = channel else {
            return Ok(None);
        };
        let exists = self.conn.execute(
            "DELETE FROM reactions WHERE message_id = ?1 AND emoji = ?2 AND user = ?3",
            params![id, emoji, user],
        )?;
        if exists == 0 {
            self.conn.execute(
                "INSERT INTO reactions (message_id, emoji, user) VALUES (?1, ?2, ?3)",
                params![id, emoji, user],
            )?;
        }
        Ok(Some((channel, self.reactions_for(&[id])?.remove(&id).unwrap_or_default())))
    }

    /// Reactions for the given messages, keyed by message id. Each emoji appears once, in the
    /// order it was first added.
    fn reactions_for(&self, ids: &[i64]) -> rusqlite::Result<std::collections::HashMap<i64, Vec<common::Reaction>>> {
        let mut out: std::collections::HashMap<i64, Vec<common::Reaction>> = Default::default();
        if ids.is_empty() {
            return Ok(out);
        }
        let names = self.display_names()?;
        let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT message_id, emoji, user FROM reactions WHERE message_id IN ({list}) ORDER BY rowid"
        ))?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?;
        for row in rows {
            let (message, emoji, user) = row?;
            // Show the reactor's name, not their id.
            let user = names.get(&user).cloned().unwrap_or(user);
            let reactions = out.entry(message).or_default();
            match reactions.iter_mut().find(|r| r.emoji == emoji) {
                Some(r) => r.users.push(user),
                None => reactions.push(common::Reaction { emoji, users: vec![user] }),
            }
        }
        Ok(out)
    }

    /// Changes the text of a message, but only when `author` wrote it. Returns the message's
    /// channel id, or `None` when there is no such message by that author.
    pub fn edit_message(&self, id: i64, author: &str, text: &str) -> rusqlite::Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        let changed = self.conn.execute(
            "UPDATE messages SET text = ?1, edited = 1 WHERE id = ?2 AND author = ?3",
            params![text, id, author],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.conn
            .query_row("SELECT channel_id FROM messages WHERE id = ?1", params![id], |row| row.get(0))
            .optional()
    }

    /// The most recent `HISTORY_LEN` messages in a channel, oldest first.
    pub fn history(&self, channel: i64) -> rusqlite::Result<Vec<ChatMessage>> {
        let name = self.channel_name(channel)?.unwrap_or_default();
        let mut stmt = self.conn.prepare(
            "SELECT id, channel_id, author, text, timestamp, edited FROM (
                 SELECT id, channel_id, author, text, timestamp, edited
                 FROM messages WHERE channel_id = ?1 ORDER BY id DESC LIMIT ?2
             ) ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![channel, HISTORY_LEN], |row| {
            Ok(ChatMessage {
                id: row.get(0)?,
                channel_id: row.get(1)?,
                channel: name.clone(),
                author: row.get(2)?,
                author_id: String::new(),
                text: row.get(3)?,
                timestamp: row.get::<_, i64>(4)? as u64,
                edited: row.get::<_, i64>(5)? != 0,
                reactions: Vec::new(),
            })
        })?;
        let mut messages: Vec<ChatMessage> = rows.collect::<rusqlite::Result<_>>()?;
        let names = self.display_names()?;
        for m in &mut messages {
            m.author_id = m.author.clone();
            m.author = names.get(&m.author_id).cloned().unwrap_or_else(|| m.author_id.clone());
        }
        let ids: Vec<i64> = messages.iter().map(|m| m.id).collect();
        let mut reactions = self.reactions_for(&ids)?;
        for m in &mut messages {
            m.reactions = reactions.remove(&m.id).unwrap_or_default();
        }
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message in a channel, by its name. The channel is created if needed.
    fn message(db: &Db, channel: &str, author: &str, text: &str) -> ChatMessage {
        ChatMessage {
            id: 0,
            channel_id: db.ensure_channel(channel).unwrap(),
            channel: channel.into(),
            author: author.into(),
            author_id: author.into(),
            text: text.into(),
            timestamp: 1,
            edited: false,
            reactions: vec![],
        }
    }

    #[test]
    fn direct_messages_are_for_their_two_members_only() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        let dm = db.open_direct_message("alice", "bob").unwrap();
        let dm_id = db.ensure_channel(&dm).unwrap();
        let general = db.ensure_channel("general").unwrap();

        let in_dm = db.save_message(&message(&db, &dm, "alice", "hi")).unwrap();
        let in_channel = db.save_message(&message(&db, "general", "alice", "hi")).unwrap();

        // The owner (not in the DM) cannot delete it; the owner can delete a channel message.
        assert_eq!(db.delete_message(in_dm, "owner", true).unwrap(), None);
        assert_eq!(db.delete_message(in_channel, "owner", true).unwrap(), Some(general));
        // The author can delete their own direct message.
        assert_eq!(db.delete_message(in_dm, "alice", false).unwrap(), Some(dm_id));
    }
}

#[cfg(test)]
mod category_tests {
    use super::*;

    #[test]
    fn categories_group_channels_and_deleting_keeps_them() {
        let db = Db::open(":memory:").unwrap();
        let general = db.ensure_channel("general").unwrap();
        let random = db.ensure_channel("random").unwrap();
        let text = db.create_category("Text").unwrap().expect("created");
        assert!(db.create_category("Text").unwrap().is_none(), "names are unique");
        db.move_channel(general, Some(text)).unwrap();
        db.move_channel(random, None).unwrap();
        let cats = db.categories().unwrap();
        assert_eq!(cats.len(), 1);
        assert_eq!(cats[0].name, "Text");
        assert_eq!(cats[0].channels, vec![common::ChannelRef { id: general, name: "general".into() }]);

        assert!(db.delete_category(text).unwrap());
        assert!(db.categories().unwrap().is_empty());
        // The channel still exists and is uncategorized.
        assert!(db.channel_list().unwrap().iter().any(|c| c.id == general));
        assert!(!db.delete_category(text).unwrap());
    }
}

#[cfg(test)]
mod rename_tests {
    use super::*;

    #[test]
    fn renaming_keeps_the_id_and_the_messages() {
        let db = Db::open(":memory:").unwrap();
        let general = db.ensure_channel("general").unwrap();
        let chat = db.ensure_channel("chat").unwrap();
        let text = db.create_category("Text").unwrap().expect("created");
        db.move_channel(general, Some(text)).unwrap();
        let m = ChatMessage {
            id: 0,
            channel_id: general,
            channel: "general".into(),
            author: "alice".into(),
            author_id: "u1".into(),
            text: "hi".into(),
            timestamp: 1,
            edited: false,
            reactions: Vec::new(),
        };
        db.save_message(&m).unwrap();

        assert!(!db.rename_channel(general, "chat").unwrap(), "the new name is taken");
        assert!(db.rename_channel(general, "lobby").unwrap());
        assert_eq!(db.channel_name(general).unwrap().as_deref(), Some("lobby"));
        assert_eq!(db.channel_id("lobby").unwrap(), Some(general), "the id stays the same");
        assert_eq!(db.history(general).unwrap().len(), 1, "messages stay with the channel");
        assert_eq!(db.categories().unwrap()[0].channels[0].name, "lobby", "the category keeps it");
        assert!(db.channel_list().unwrap().iter().any(|c| c.id == chat));
    }

    #[test]
    fn deleting_a_channel_removes_its_messages_and_reorders_categories() {
        let db = Db::open(":memory:").unwrap();
        let general = db.ensure_channel("general").unwrap();
        let m = ChatMessage {
            id: 0,
            channel_id: general,
            channel: "general".into(),
            author: "alice".into(),
            author_id: "u1".into(),
            text: "bye".into(),
            timestamp: 1,
            edited: false,
            reactions: Vec::new(),
        };
        db.save_message(&m).unwrap();
        assert!(db.delete_channel(general).unwrap());
        assert!(db.channel_name(general).unwrap().is_none());
        assert!(!db.delete_channel(general).unwrap());

        let a = db.create_category("A").unwrap().unwrap();
        let b = db.create_category("B").unwrap().unwrap();
        let c = db.create_category("C").unwrap().unwrap();
        assert!(db.move_category(c, Some(a)).unwrap());
        let order: Vec<String> = db.categories().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(order, vec!["C", "A", "B"]);
        assert!(db.move_category(a, None).unwrap());
        let order: Vec<String> = db.categories().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(order, vec!["C", "B", "A"]);
        assert!(!db.move_category(b, Some(b)).unwrap());
    }

    #[test]
    fn channels_can_be_ordered_within_a_category() {
        let db = Db::open(":memory:").unwrap();
        let cat = db.create_category("Work").unwrap().unwrap();
        let a = db.ensure_channel("aaa").unwrap();
        let b = db.ensure_channel("bbb").unwrap();
        let c = db.ensure_channel("ccc").unwrap();
        db.move_channel(a, Some(cat)).unwrap();
        db.move_channel(b, Some(cat)).unwrap();
        db.move_channel(c, Some(cat)).unwrap();
        let names = |db: &Db| -> Vec<String> {
            db.categories().unwrap().into_iter().find(|x| x.id == cat).unwrap().channels.into_iter().map(|c| c.name).collect()
        };
        assert_eq!(names(&db), vec!["aaa", "bbb", "ccc"]);
        // Drop ccc before aaa.
        assert!(db.place_channel(c, Some(cat), Some(a)).unwrap());
        assert_eq!(names(&db), vec!["ccc", "aaa", "bbb"]);
        // Drop aaa at the end.
        assert!(db.place_channel(a, Some(cat), None).unwrap());
        assert_eq!(names(&db), vec!["ccc", "bbb", "aaa"]);
        // Uncategorized channels keep their own order.
        assert!(db.place_channel(b, None, None).unwrap());
        assert!(db.uncategorized_channels().unwrap().iter().any(|x| x.id == b));
    }

    #[test]
    fn direct_messages_are_not_listed_as_channels() {
        let db = Db::open(":memory:").unwrap();
        db.ensure_channel("general").unwrap();
        let dm = db.ensure_channel("dm-9").unwrap();
        let listed = db.channel_list().unwrap();
        assert!(listed.iter().any(|c| c.name == "general"));
        assert!(listed.iter().all(|c| !c.name.starts_with("dm-")), "no direct message is listed");
        assert!(dm > 0, "the direct message still has a row for its messages");
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// A database in the layout from before channels and categories had IDs.
    #[test]
    fn an_old_database_keeps_its_channels_messages_and_categories() {
        let path = std::env::temp_dir().join(format!("vaxelium-migrate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE channels (name TEXT PRIMARY KEY, category TEXT);
                 CREATE TABLE categories (name TEXT PRIMARY KEY, position INTEGER NOT NULL);
                 CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, channel TEXT NOT NULL,
                     author TEXT NOT NULL, text TEXT NOT NULL, timestamp INTEGER NOT NULL);
                 INSERT INTO categories (name, position) VALUES ('Text', 1);
                 INSERT INTO channels (name, category) VALUES ('general', 'Text'), ('random', NULL);
                 INSERT INTO messages (channel, author, text, timestamp)
                     VALUES ('general', 'u1', 'old message', 5), ('dm-7', 'u1', 'old dm', 6);",
            )
            .unwrap();
        }
        let db = Db::open(path.to_str().unwrap()).unwrap();
        let general = db.channel_id("general").unwrap().expect("kept");
        let history = db.history(general).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text, "old message");
        assert_eq!(db.channel_id("dm-7").unwrap().is_some(), true, "message channels get a row");
        let cats = db.categories().unwrap();
        assert_eq!(cats[0].name, "Text");
        assert_eq!(cats[0].channels[0].name, "general", "the category is kept");
        assert!(db.info_value("server_id").unwrap().is_some(), "the server gets an id");
        // Opening again changes nothing.
        drop(db);
        let db = Db::open(path.to_str().unwrap()).unwrap();
        assert_eq!(db.categories().unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod member_id_tests {
    use super::*;

    #[test]
    fn stored_legacy_ids_are_found_and_replaced() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("alice", "Alice", 1).unwrap();
        let dm = db.open_direct_message("alice", "bob").unwrap();
        let general = db.ensure_channel("general").unwrap();
        db.save_message(&ChatMessage {
            id: 0,
            channel_id: general,
            channel: "general".into(),
            author: "alice".into(),
            author_id: "alice".into(),
            text: "hi".into(),
            timestamp: 1,
            edited: false,
            reactions: Vec::new(),
        })
        .unwrap();
        assert!(db.legacy_ids().unwrap().contains(&"alice".to_string()));

        let new_id = "0123456789abcdef0123456789abcdef".to_string();
        let changed = db.apply_id_map(&[("alice".to_string(), new_id.clone())]).unwrap();
        assert!(changed > 0);
        assert!(!db.legacy_ids().unwrap().contains(&"alice".to_string()), "no old id is left");
        assert_eq!(db.history(general).unwrap()[0].author_id, new_id, "messages follow the new id");
        assert!(db.is_direct_message_member(&dm, &new_id).unwrap(), "direct messages follow it too");
    }
}

#[cfg(test)]
mod default_channel_tests {
    use super::*;

    #[test]
    fn a_deleted_default_channel_stays_deleted_after_a_restart() {
        let path = std::env::temp_dir().join(format!("vaxelium-defaults-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path_str = path.to_str().unwrap();
        {
            let db = Db::open(path_str).unwrap();
            assert!(db.channel_id("general").unwrap().is_some(), "a new database has the defaults");
            let general = db.channel_id("general").unwrap().unwrap();
            assert!(db.delete_channel(general).unwrap());
        }
        let db = Db::open(path_str).unwrap();
        assert!(db.channel_id("general").unwrap().is_none(), "restarting does not bring it back");
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod resurrect_tests {
    use super::*;

    #[test]
    fn a_deleted_channel_is_not_recreated_from_old_messages_on_boot() {
        let path = std::env::temp_dir().join(format!("vaxelium-resurrect-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path_str = path.to_str().unwrap();
        {
            let db = Db::open(path_str).unwrap();
            let general = db.channel_id("general").unwrap().unwrap();
            // A message written by an older server: it has the channel's name but no channel id.
            db.conn
                .execute(
                    "INSERT INTO messages (channel, author, text, timestamp) VALUES ('general', 'u1', 'old', 1)",
                    [],
                )
                .unwrap();
            assert!(db.delete_channel(general).unwrap());
        }
        let db = Db::open(path_str).unwrap();
        assert!(db.channel_id("general").unwrap().is_none(), "boot brought general back");
        let _ = std::fs::remove_file(&path);
    }
}
