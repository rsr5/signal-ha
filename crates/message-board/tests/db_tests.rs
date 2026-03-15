use message_board::db::Pool;
use tempfile::TempDir;

fn test_pool() -> (Pool, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.db");
    let pool = Pool::open(path.to_str().unwrap()).unwrap();
    pool.migrate().unwrap();
    (pool, dir)
}

// ── Post CRUD ──────────────────────────────────────────────────────

#[test]
fn create_post_returns_with_id_and_defaults() {
    let (pool, _dir) = test_pool();
    let post = pool.create_post("porch-lights", "Lux sensor offline").unwrap();

    assert_eq!(post.id, 1);
    assert_eq!(post.agent, "porch-lights");
    assert_eq!(post.body, "Lux sensor offline");
    assert!(post.active);
    // Replies included on get_post (create uses get_post_inner)
    assert!(post.replies.unwrap().is_empty());
}

#[test]
fn auto_increment_ids() {
    let (pool, _dir) = test_pool();
    let p1 = pool.create_post("a", "one").unwrap();
    let p2 = pool.create_post("b", "two").unwrap();
    let p3 = pool.create_post("a", "three").unwrap();

    assert_eq!(p1.id, 1);
    assert_eq!(p2.id, 2);
    assert_eq!(p3.id, 3);
}

#[test]
fn get_post_includes_empty_replies() {
    let (pool, _dir) = test_pool();
    pool.create_post("garage", "motion stuck").unwrap();

    let post = pool.get_post(1).unwrap();
    assert_eq!(post.replies.unwrap().len(), 0);
}

#[test]
fn get_post_not_found() {
    let (pool, _dir) = test_pool();
    assert!(pool.get_post(999).is_err());
}

// ── Filtering ──────────────────────────────────────────────────────

#[test]
fn list_posts_no_filter_returns_all() {
    let (pool, _dir) = test_pool();
    pool.create_post("a", "one").unwrap();
    pool.create_post("b", "two").unwrap();
    pool.create_post("a", "three").unwrap();

    let all = pool.list_posts(None, None).unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn list_posts_filter_by_agent() {
    let (pool, _dir) = test_pool();
    pool.create_post("porch", "one").unwrap();
    pool.create_post("garage", "two").unwrap();
    pool.create_post("porch", "three").unwrap();

    let porch = pool.list_posts(Some("porch"), None).unwrap();
    assert_eq!(porch.len(), 2);
    assert!(porch.iter().all(|p| p.agent == "porch"));
}

#[test]
fn list_posts_filter_by_active() {
    let (pool, _dir) = test_pool();
    pool.create_post("a", "one").unwrap();
    pool.create_post("a", "two").unwrap();
    pool.update_post(1, Some(false), None).unwrap();

    let active = pool.list_posts(None, Some(true)).unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, 2);

    let inactive = pool.list_posts(None, Some(false)).unwrap();
    assert_eq!(inactive.len(), 1);
    assert_eq!(inactive[0].id, 1);
}

#[test]
fn list_posts_filter_agent_and_active() {
    let (pool, _dir) = test_pool();
    pool.create_post("porch", "one").unwrap();
    pool.create_post("garage", "two").unwrap();
    pool.create_post("porch", "three").unwrap();
    pool.update_post(1, Some(false), None).unwrap();

    let result = pool.list_posts(Some("porch"), Some(true)).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].body, "three");
}

#[test]
fn list_posts_ordered_newest_first() {
    let (pool, _dir) = test_pool();
    pool.create_post("a", "first").unwrap();
    pool.create_post("a", "second").unwrap();
    pool.create_post("a", "third").unwrap();

    let all = pool.list_posts(None, None).unwrap();
    assert_eq!(all[0].body, "third");
    assert_eq!(all[2].body, "first");
}

// ── Update ─────────────────────────────────────────────────────────

#[test]
fn update_post_deactivate() {
    let (pool, _dir) = test_pool();
    pool.create_post("a", "issue").unwrap();

    let updated = pool.update_post(1, Some(false), None).unwrap();
    assert!(!updated.active);
    assert_eq!(updated.body, "issue");

    // Confirm persistence
    let fetched = pool.get_post(1).unwrap();
    assert!(!fetched.active);
}

#[test]
fn update_post_reactivate() {
    let (pool, _dir) = test_pool();
    pool.create_post("a", "issue").unwrap();
    pool.update_post(1, Some(false), None).unwrap();
    let reactivated = pool.update_post(1, Some(true), None).unwrap();
    assert!(reactivated.active);
}

#[test]
fn update_post_body() {
    let (pool, _dir) = test_pool();
    pool.create_post("a", "original").unwrap();

    let updated = pool.update_post(1, None, Some("amended")).unwrap();
    assert_eq!(updated.body, "amended");
    assert!(updated.active); // unchanged
}

#[test]
fn update_post_both_fields() {
    let (pool, _dir) = test_pool();
    pool.create_post("a", "original").unwrap();

    let updated = pool.update_post(1, Some(false), Some("closed")).unwrap();
    assert!(!updated.active);
    assert_eq!(updated.body, "closed");
}

#[test]
fn update_nonexistent_post_errors() {
    let (pool, _dir) = test_pool();
    assert!(pool.update_post(999, Some(false), None).is_err());
}

// ── Replies ────────────────────────────────────────────────────────

#[test]
fn create_reply_and_fetch_with_post() {
    let (pool, _dir) = test_pool();
    pool.create_post("porch", "lux sensor seems off").unwrap();

    let r1 = pool.create_reply(1, "user", "I replaced the sensor").unwrap();
    assert_eq!(r1.post_id, 1);
    assert_eq!(r1.author, "user");

    let r2 = pool
        .create_reply(1, "porch-agent", "Confirmed, readings normal now")
        .unwrap();
    assert_eq!(r2.id, 2);

    let post = pool.get_post(1).unwrap();
    let replies = post.replies.unwrap();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0].author, "user");
    assert_eq!(replies[1].author, "porch-agent");
}

#[test]
fn reply_to_nonexistent_post_errors() {
    let (pool, _dir) = test_pool();
    assert!(pool.create_reply(999, "user", "hello").is_err());
}

#[test]
fn replies_ordered_chronologically() {
    let (pool, _dir) = test_pool();
    pool.create_post("a", "issue").unwrap();

    pool.create_reply(1, "user", "first").unwrap();
    pool.create_reply(1, "agent", "second").unwrap();
    pool.create_reply(1, "user", "third").unwrap();

    let post = pool.get_post(1).unwrap();
    let replies = post.replies.unwrap();
    assert_eq!(replies[0].body, "first");
    assert_eq!(replies[1].body, "second");
    assert_eq!(replies[2].body, "third");
}

#[test]
fn replies_isolated_between_posts() {
    let (pool, _dir) = test_pool();
    pool.create_post("a", "post one").unwrap();
    pool.create_post("b", "post two").unwrap();

    pool.create_reply(1, "user", "reply to one").unwrap();
    pool.create_reply(2, "user", "reply to two").unwrap();
    pool.create_reply(2, "user", "another to two").unwrap();

    let p1 = pool.get_post(1).unwrap();
    assert_eq!(p1.replies.unwrap().len(), 1);

    let p2 = pool.get_post(2).unwrap();
    assert_eq!(p2.replies.unwrap().len(), 2);
}

// ── List does NOT include replies (performance) ────────────────────

#[test]
fn list_posts_omits_replies() {
    let (pool, _dir) = test_pool();
    pool.create_post("a", "issue").unwrap();
    pool.create_reply(1, "user", "hello").unwrap();

    let posts = pool.list_posts(None, None).unwrap();
    assert!(posts[0].replies.is_none());
}

// ── Migration idempotency ──────────────────────────────────────────

#[test]
fn migrate_twice_is_safe() {
    let (pool, _dir) = test_pool();
    pool.migrate().unwrap(); // second time
    pool.create_post("a", "should work").unwrap();
}

// ── Concurrent-ish writes ──────────────────────────────────────────

#[test]
fn many_posts_from_multiple_agents() {
    let (pool, _dir) = test_pool();

    let agents = ["porch", "garage", "office", "living-room", "bedroom-1"];
    for agent in &agents {
        for i in 0..20 {
            pool.create_post(agent, &format!("finding {i}")).unwrap();
        }
    }

    let all = pool.list_posts(None, None).unwrap();
    assert_eq!(all.len(), 100);

    for agent in &agents {
        let filtered = pool.list_posts(Some(agent), None).unwrap();
        assert_eq!(filtered.len(), 20);
    }
}

// ── Unicode and special characters ─────────────────────────────────

#[test]
fn unicode_content() {
    let (pool, _dir) = test_pool();
    let post = pool
        .create_post("porch", "💡 Lux sensor: 日本語テスト — ñoño")
        .unwrap();
    assert_eq!(post.body, "💡 Lux sensor: 日本語テスト — ñoño");

    let reply = pool
        .create_reply(1, "user", "🔧 Fixed! Ça marche")
        .unwrap();
    assert_eq!(reply.body, "🔧 Fixed! Ça marche");
}

#[test]
fn large_body() {
    let (pool, _dir) = test_pool();
    let big = "x".repeat(100_000);
    let post = pool.create_post("agent", &big).unwrap();
    assert_eq!(post.body.len(), 100_000);
}

#[test]
fn empty_body_allowed() {
    let (pool, _dir) = test_pool();
    let post = pool.create_post("agent", "").unwrap();
    assert_eq!(post.body, "");
}
