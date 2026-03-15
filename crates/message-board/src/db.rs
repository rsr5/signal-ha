use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};
use serde::Serialize;

/// Thread-safe SQLite connection pool (single writer).
pub struct Pool {
    conn: Mutex<Connection>,
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct Post {
    pub id: i64,
    pub agent: String,
    pub body: String,
    pub active: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replies: Option<Vec<Reply>>,
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct Reply {
    pub id: i64,
    pub post_id: i64,
    pub author: String,
    pub body: String,
    pub created_at: String,
}

impl Pool {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "foreign_keys", "on")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS posts (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                agent      TEXT NOT NULL,
                body       TEXT NOT NULL,
                active     INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
            );
            CREATE INDEX IF NOT EXISTS idx_posts_agent_active ON posts(agent, active);

            CREATE TABLE IF NOT EXISTS replies (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                post_id    INTEGER NOT NULL REFERENCES posts(id),
                author     TEXT NOT NULL,
                body       TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
            );
            CREATE INDEX IF NOT EXISTS idx_replies_post ON replies(post_id);",
        )?;
        Ok(())
    }

    /// Create a new post. Returns the created post.
    pub fn create_post(&self, agent: &str, body: &str) -> anyhow::Result<Post> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO posts (agent, body) VALUES (?1, ?2)",
            params![agent, body],
        )?;
        let id = conn.last_insert_rowid();
        self.get_post_inner(&conn, id)
    }

    /// List posts with optional filters.
    pub fn list_posts(
        &self,
        agent: Option<&str>,
        active: Option<bool>,
    ) -> anyhow::Result<Vec<Post>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = "SELECT id, agent, body, active, created_at, updated_at FROM posts WHERE 1=1".to_string();
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(a) = agent {
            sql.push_str(" AND agent = ?");
            params_vec.push(Box::new(a.to_string()));
        }
        if let Some(act) = active {
            sql.push_str(" AND active = ?");
            params_vec.push(Box::new(act as i32));
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        let mut stmt = conn.prepare(&sql)?;
        let posts = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(Post {
                    id: row.get(0)?,
                    agent: row.get(1)?,
                    body: row.get(2)?,
                    active: row.get::<_, i32>(3)? != 0,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    replies: None,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(posts)
    }

    /// Get a single post with its replies.
    pub fn get_post(&self, id: i64) -> anyhow::Result<Post> {
        let conn = self.conn.lock().unwrap();
        self.get_post_inner(&conn, id)
    }

    fn get_post_inner(&self, conn: &Connection, id: i64) -> anyhow::Result<Post> {
        let mut post = conn.query_row(
            "SELECT id, agent, body, active, created_at, updated_at FROM posts WHERE id = ?1",
            params![id],
            |row| {
                Ok(Post {
                    id: row.get(0)?,
                    agent: row.get(1)?,
                    body: row.get(2)?,
                    active: row.get::<_, i32>(3)? != 0,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    replies: None,
                })
            },
        )?;

        let mut stmt = conn.prepare(
            "SELECT id, post_id, author, body, created_at FROM replies WHERE post_id = ?1 ORDER BY created_at ASC",
        )?;
        let replies = stmt
            .query_map(params![id], |row| {
                Ok(Reply {
                    id: row.get(0)?,
                    post_id: row.get(1)?,
                    author: row.get(2)?,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        post.replies = Some(replies);
        Ok(post)
    }

    /// Update a post (active status and/or body).
    pub fn update_post(
        &self,
        id: i64,
        active: Option<bool>,
        body: Option<&str>,
    ) -> anyhow::Result<Post> {
        let conn = self.conn.lock().unwrap();
        if let Some(act) = active {
            conn.execute(
                "UPDATE posts SET active = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?2",
                params![act as i32, id],
            )?;
        }
        if let Some(b) = body {
            conn.execute(
                "UPDATE posts SET body = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?2",
                params![b, id],
            )?;
        }
        self.get_post_inner(&conn, id)
    }

    /// Add a reply to a post.
    pub fn create_reply(&self, post_id: i64, author: &str, body: &str) -> anyhow::Result<Reply> {
        let conn = self.conn.lock().unwrap();
        // Verify post exists
        conn.query_row(
            "SELECT id FROM posts WHERE id = ?1",
            params![post_id],
            |_| Ok(()),
        )?;

        conn.execute(
            "INSERT INTO replies (post_id, author, body) VALUES (?1, ?2, ?3)",
            params![post_id, author, body],
        )?;
        let id = conn.last_insert_rowid();
        let reply = conn.query_row(
            "SELECT id, post_id, author, body, created_at FROM replies WHERE id = ?1",
            params![id],
            |row| {
                Ok(Reply {
                    id: row.get(0)?,
                    post_id: row.get(1)?,
                    author: row.get(2)?,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                })
            },
        )?;
        Ok(reply)
    }
}
