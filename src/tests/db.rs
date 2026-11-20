mod tests {
    use super::super::*;

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
            reply: None,
            pinned: false,
            report: None,
            image: None,
        }
    }

    #[test]
    fn a_renamed_member_shows_the_new_name_to_everyone() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("acct-1", "Old name", 1).unwrap();
        db.add_member("acct-1", "New name", 2).unwrap();
        assert_eq!(db.display_names().unwrap().get("acct-1").map(String::as_str), Some("New name"));
    }

    #[test]
    fn the_automod_role_is_always_an_administrator() {
        let db = Db::open(":memory:").unwrap();
        db.ensure_automod_role().unwrap();
        let role = db.role_list().unwrap().into_iter().find(|r| r.id == common::AUTOMOD_ROLE).unwrap();
        assert_eq!(role.permissions & common::perm::ADMINISTRATOR, common::perm::ADMINISTRATOR);
    }

    #[test]
    fn the_automod_role_is_listed_last_below_everyone() {
        let db = Db::open(":memory:").unwrap();
        db.ensure_automod_role().unwrap();
        db.create_role("Founder", "#1abc9c", 0, false, false).unwrap();
        let roles = db.role_list().unwrap();
        let ids: Vec<i64> = roles.iter().map(|r| r.id).collect();
        assert_eq!(ids.last(), Some(&common::AUTOMOD_ROLE), "AutoMod is the last role: {ids:?}");
        assert_eq!(ids[ids.len() - 2], common::EVERYONE_ROLE, "@everyone is just above it");
    }

    #[test]
    fn the_automod_role_is_made_once_and_never_clashes_with_new_roles() {
        let db = Db::open(":memory:").unwrap();
        db.ensure_automod_role().unwrap();
        db.ensure_automod_role().unwrap();
        let roles = db.role_list().unwrap();
        let automod: Vec<_> = roles.iter().filter(|r| r.id == common::AUTOMOD_ROLE).collect();
        assert_eq!(automod.len(), 1, "made once");
        assert_eq!(automod[0].name, "AutoMod");
        let made = db.create_role("Mods", "", 0, false, false).unwrap().expect("made");
        assert!(made > 0, "a new role gets a positive id, not the AutoMod id");
    }

    #[test]
    fn the_system_roles_come_after_everyone_in_alphabetical_order() {
        let db = Db::open(":memory:").unwrap();
        db.ensure_automod_role().unwrap();
        db.ensure_system_role().unwrap();
        let roles = db.role_list().unwrap();
        let system = roles.iter().find(|r| r.id == common::SYSTEM_ROLE).unwrap();
        assert_eq!(system.permissions & common::perm::ADMINISTRATOR, common::perm::ADMINISTRATOR);
        let ids: Vec<i64> = roles.iter().map(|r| r.id).collect();
        let n = ids.len();
        assert_eq!(&ids[n - 3..], &[common::EVERYONE_ROLE, common::AUTOMOD_ROLE, common::SYSTEM_ROLE]);
    }

    #[test]
    fn a_join_notice_keeps_its_picture_in_the_history() {
        let db = Db::open(":memory:").unwrap();
        let welcome = db.ensure_channel("welcome").unwrap();
        let picture = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"\x89PNG fake bytes");
        let mut notice = message(&db, "welcome", &common::notice_author("dummy"), "Dummy joined the server");
        notice.image = Some(picture.clone());
        db.save_message(&notice).unwrap();
        db.save_message(&message(&db, "welcome", "bob", "hi")).unwrap();
        let history = db.history(welcome).unwrap();
        let with_picture: Vec<_> = history.iter().filter(|m| m.image.is_some()).collect();
        assert_eq!(with_picture.len(), 1);
        assert_eq!(with_picture[0].image.as_deref(), Some(picture.as_str()));
        assert!(history.iter().filter(|m| m.image.is_none()).count() == 1);
    }

    #[test]
    fn connect_notices_are_kept_in_the_channel_history() {
        let db = Db::open(":memory:").unwrap();
        let welcome = db.ensure_channel("welcome").unwrap();
        let mut notice = message(&db, "welcome", &common::notice_author("dummy"), "Dummy connected");
        notice.author = "System".into();
        db.save_message(&notice).unwrap();
        let history = db.history(welcome).unwrap();
        assert_eq!(history.len(), 1);
        assert!(common::is_notice(&history[0].author_id));
        assert_eq!(history[0].text, "Dummy connected");
        // The notice is posted by the server, so it is not listed as a member, the same as AutoMod.
        assert!(!db.member_names().unwrap().iter().any(|n| common::is_notice(n)));
        assert!(!db.member_names().unwrap().contains(&common::AUTOMOD_ID.to_string()));
        assert!(common::is_system_id("system:anything"));
        assert!(!common::is_system_id("0123abcd"));
    }

    #[test]
    fn automod_reports_are_kept_with_the_message() {
        let db = Db::open(":memory:").unwrap();
        let general = db.ensure_channel("general").unwrap();
        let mut alert = message(&db, "general", common::AUTOMOD_ID, "Blocked a message");
        alert.author = "AutoMod".into();
        alert.report = Some(common::AutoModReport {
            user_id: "id-dummy".into(),
            display: "Dummy".into(),
            channel_id: general,
            channel: "general".into(),
            rule: "AutoMod Flagged Words".into(),
            keyword: "twat".into(),
            content: "your a twat mate".into(),
        });
        db.save_message(&alert).unwrap();
        db.save_message(&message(&db, "general", "bob", "hello")).unwrap();
        let history = db.history(general).unwrap();
        assert_eq!(history[0].report, alert.report, "the report comes back with the alert");
        assert!(history[1].report.is_none(), "ordinary messages have none");
    }

    #[test]
    fn automod_is_not_a_member() {
        let db = Db::open(":memory:").unwrap();
        db.save_message(&message(&db, "general", "alice", "hi")).unwrap();
        db.save_message(&message(&db, "general", common::AUTOMOD_ID, "Flagged a name."))
            .unwrap();
        let members = db.member_names().unwrap();
        assert!(members.contains(&"alice".to_string()));
        assert!(
            !members.contains(&common::AUTOMOD_ID.to_string()),
            "AutoMod is part of the server"
        );
    }

    #[test]
    fn ownership_passes_to_another_member() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("bob", "Bob", 2).unwrap();
        db.transfer_ownership("bob").unwrap();
        assert_eq!(db.owner().unwrap().as_deref(), Some("bob"));
    }

    #[test]
    fn deleting_the_server_removes_everything_and_refuses_later_connections() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("bob", "Bob", 2).unwrap();
        assert!(!db.is_deleted().unwrap());
        db.delete_server().unwrap();
        assert!(db.is_deleted().unwrap());
        assert!(db.member_names().unwrap().is_empty(), "no members are left");
        assert!(db.channel_list().unwrap().is_empty(), "no channels are left");
        assert_eq!(db.owner().unwrap(), None, "no owner is left");
    }

    #[test]
    fn joining_again_after_leaving_is_a_first_join() {
        // The join notice is posted when the member is not yet in the members table. Leaving removes
        // the row, so joining again finds the member not joined and posts the notice again.
        let db = Db::open(":memory:").unwrap();
        db.add_member("bob", "Bob", 1).unwrap();
        assert!(db.is_member("bob").unwrap());
        db.leave_server("bob").unwrap();
        assert!(!db.is_member("bob").unwrap());
        db.clear_left("bob").unwrap();
        assert!(!db.is_member("bob").unwrap());
    }

    #[test]
    fn leaving_removes_the_membership_and_what_it_held() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("bob", "Bob", 2).unwrap();
        // Bob has posted, which used to keep him in the member list after he left.
        db.save_message(&message(&db, "general", "bob", "hello")).unwrap();
        db.leave_server("bob").unwrap();
        let members = db.member_names().unwrap();
        assert!(members.contains(&"owner".to_string()));
        assert!(!members.contains(&"bob".to_string()), "bob is no longer a member");
        assert!(db.has_left("bob").unwrap(), "bob stays out until asked back");
        assert_eq!(
            db.display_names().unwrap().get("bob").map(String::as_str),
            Some("Bob"),
            "bob's messages still show his name"
        );
        db.clear_left("bob").unwrap();
        assert!(!db.has_left("bob").unwrap());
    }

    #[test]
    fn system_messages_default_to_connection_notices_in_the_member_channel() {
        let db = Db::open(":memory:").unwrap();
        assert_eq!(db.system_messages().unwrap(), common::SystemMessages::default());
        let general = db.channel_id("general").unwrap().unwrap();
        let settings = common::SystemMessages {
            connect_notice: false,
            channel_id: Some(general),
        };
        db.set_system_messages(&settings).unwrap();
        assert_eq!(db.system_messages().unwrap(), settings);
    }

    #[test]
    fn automod_rules_are_kept() {
        let db = Db::open(":memory:").unwrap();
        assert_eq!(
            db.automod().unwrap(),
            common::AutoMod::default(),
            "nothing is switched on at first"
        );
        let rules = common::AutoMod {
            words: common::WordRule {
                enabled: true,
                words: vec!["bad".into()],
                ..common::WordRule::default()
            },
            block_links: true,
            spam_filter: true,
            names: common::NameRule {
                enabled: true,
                words: vec!["*admin*".into()],
                block_interactions: true,
                send_alert: true,
                allowed_roles: vec![2],
                ..common::NameRule::default()
            },
            alert_channel: Some(3),
            mentions: common::MentionRule {
                enabled: true,
                max_unique: 3,
                raid_detection: true,
                timeout_secs: 60,
                block: false,
                send_alert: false,
                allowed_roles: vec![2],
                notice: "Slow down with the mentions".into(),
            },
            flagged: common::FlaggedWords {
                enabled: true,
                severe_profanity: true,
                general_insults: false,
                identity_slurs: true,
                mild_profanity: true,
                sexual_content: true,
                sexual_harassment: true,
                threats: true,
                spam: false,
                allow: vec!["porn star".into()],
                block: true,
                send_alert: false,
                notice: "Watch your language".into(),
                allowed_roles: vec![4],
                allowed_channels: vec![5],
            },
            filtered: common::FilteredWords {
                enabled: true,
                patterns: vec!["bad[0-9]+".into()],
                repeated_chars: true,
                repeated_limit: 4,
                block: false,
                send_alert: true,
                notice: "No filters here".into(),
                allowed_roles: vec![6],
                allowed_channels: vec![7],
            },
        };
        db.set_automod(&rules).unwrap();
        assert_eq!(db.automod().unwrap(), rules);
    }

    #[test]
    fn pins_are_listed_and_capped() {
        let db = Db::open(":memory:").unwrap();
        let general = db.ensure_channel("general").unwrap();
        let first = db.save_message(&message(&db, "general", "alice", "first")).unwrap();
        let second = db.save_message(&message(&db, "general", "bob", "second")).unwrap();

        assert_eq!(db.set_pinned(first, true, 10).unwrap(), Ok(Some(general)));
        assert_eq!(db.set_pinned(second, true, 20).unwrap(), Ok(Some(general)));
        let pins: Vec<i64> = db.pins(general).unwrap().iter().map(|m| m.id).collect();
        assert_eq!(pins, vec![second, first], "the latest pin comes first");
        assert!(db.history(general).unwrap().iter().all(|m| m.pinned));

        // Pinning again keeps its place; unpinning takes it off the list.
        db.set_pinned(first, true, 30).unwrap().unwrap();
        assert_eq!(db.pins(general).unwrap()[0].id, second);
        db.set_pinned(first, false, 40).unwrap().unwrap();
        assert_eq!(db.pins(general).unwrap().len(), 1);
        assert_eq!(db.set_pinned(9999, true, 50).unwrap(), Ok(None), "no such message");

        // A full channel refuses another pin.
        for i in 1..common::MAX_PINS {
            let id = db
                .save_message(&message(&db, "general", "alice", &format!("m{i}")))
                .unwrap();
            db.set_pinned(id, true, 100 + i as u64).unwrap().unwrap();
        }
        assert_eq!(db.pins(general).unwrap().len(), common::MAX_PINS);
        assert_eq!(db.set_pinned(first, true, 999).unwrap(), Err(PinsFull));
    }

    #[test]
    fn replies_keep_their_original() {
        let db = Db::open(":memory:").unwrap();
        let general = db.ensure_channel("general").unwrap();
        let other = db.ensure_channel("other").unwrap();
        let original = db
            .save_message(&message(&db, "general", "alice", "the\noriginal"))
            .unwrap();

        // A reply may only answer a message in its own channel.
        assert!(db.reply_ref(original, other).unwrap().is_none());
        let reply = db.reply_ref(original, general).unwrap().expect("same channel");
        assert_eq!(reply.text, "the original", "the preview is on one line");

        let mut answer = message(&db, "general", "bob", "answer");
        answer.reply = Some(reply);
        db.save_message(&answer).unwrap();
        let history = db.history(general).unwrap();
        let loaded = history.last().unwrap().reply.clone().expect("the reply is kept");
        assert_eq!(
            (loaded.id, loaded.author_id.as_str(), loaded.text.as_str()),
            (original, "alice", "the original")
        );

        // Once the original is deleted, the reply still knows which message it answered, but not its text.
        db.delete_message(original, "alice", false).unwrap();
        let loaded = db
            .history(general)
            .unwrap()
            .last()
            .unwrap()
            .reply
            .clone()
            .expect("still a reply");
        assert_eq!(loaded.id, original);
        assert!(loaded.author_id.is_empty() && loaded.text.is_empty());
    }
}

mod category_tests {
    use super::super::*;

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
        assert_eq!(
            cats[0].channels,
            vec![common::ChannelRef {
                id: general,
                name: "general".into()
            }]
        );

        assert!(db.delete_category(text).unwrap());
        assert!(db.categories().unwrap().is_empty());
        // The channel still exists and is uncategorized.
        assert!(db.channel_list().unwrap().iter().any(|c| c.id == general));
        assert!(!db.delete_category(text).unwrap());
    }
}

mod rename_tests {
    use super::super::*;

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
            reply: None,
            pinned: false,
            report: None,
            image: None,
        };
        db.save_message(&m).unwrap();

        assert!(!db.rename_channel(general, "chat").unwrap(), "the new name is taken");
        assert!(db.rename_channel(general, "lobby").unwrap());
        assert_eq!(db.channel_name(general).unwrap().as_deref(), Some("lobby"));
        assert_eq!(db.channel_id("lobby").unwrap(), Some(general), "the id stays the same");
        assert_eq!(db.history(general).unwrap().len(), 1, "messages stay with the channel");
        assert_eq!(
            db.categories().unwrap()[0].channels[0].name,
            "lobby",
            "the category keeps it"
        );
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
            reply: None,
            pinned: false,
            report: None,
            image: None,
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
            db.categories()
                .unwrap()
                .into_iter()
                .find(|x| x.id == cat)
                .unwrap()
                .channels
                .into_iter()
                .map(|c| c.name)
                .collect()
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
        assert!(
            listed.iter().all(|c| !c.name.starts_with("dm-")),
            "no direct message is listed"
        );
        assert!(dm > 0, "the direct message still has a row for its messages");
    }
}

mod migration_tests {
    use super::super::*;

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
        assert_eq!(
            db.channel_id("dm-7").unwrap().is_some(),
            true,
            "message channels get a row"
        );
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

mod member_id_tests {
    use super::super::*;

    #[test]
    fn stored_legacy_ids_are_found_and_replaced() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("alice", "Alice", 1).unwrap();
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
            reply: None,
            pinned: false,
            report: None,
            image: None,
        })
        .unwrap();
        assert!(db.legacy_ids().unwrap().contains(&"alice".to_string()));

        let new_id = "0123456789abcdef0123456789abcdef".to_string();
        let changed = db.apply_id_map(&[("alice".to_string(), new_id.clone())]).unwrap();
        assert!(changed > 0);
        assert!(
            !db.legacy_ids().unwrap().contains(&"alice".to_string()),
            "no old id is left"
        );
        assert_eq!(
            db.history(general).unwrap()[0].author_id,
            new_id,
            "messages follow the new id"
        );
    }
}

mod default_channel_tests {
    use super::super::*;

    #[test]
    fn a_deleted_default_channel_stays_deleted_after_a_restart() {
        let path = std::env::temp_dir().join(format!("vaxelium-defaults-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path_str = path.to_str().unwrap();
        {
            let db = Db::open(path_str).unwrap();
            assert!(
                db.channel_id("general").unwrap().is_some(),
                "a new database has the defaults"
            );
            let general = db.channel_id("general").unwrap().unwrap();
            assert!(db.delete_channel(general).unwrap());
        }
        let db = Db::open(path_str).unwrap();
        assert!(
            db.channel_id("general").unwrap().is_none(),
            "restarting does not bring it back"
        );
        let _ = std::fs::remove_file(&path);
    }
}

mod resurrect_tests {
    use super::super::*;

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

mod category_permission_tests {
    use super::super::*;

    fn everyone_view(db: &Db, channel: i64) -> Option<Option<bool>> {
        db.channel_overrides()
            .unwrap()
            .into_iter()
            .find(|o| o.channel_id == channel && o.role == common::EVERYONE_ROLE)
            .map(|o| o.view)
    }

    #[test]
    fn setting_a_category_permission_sets_it_on_every_channel_in_the_category() {
        let db = Db::open(":memory:").unwrap();
        let a = db.ensure_channel("a").unwrap();
        let b = db.ensure_channel("b").unwrap();
        let cat = db.create_category("Group").unwrap().expect("made");
        db.move_channel(a, Some(cat)).unwrap();
        db.move_channel(b, Some(cat)).unwrap();
        db.set_category_override(
            cat,
            common::EVERYONE_ROLE,
            common::Setting {
                view: Some(false),
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(everyone_view(&db, a), Some(Some(false)));
        assert_eq!(everyone_view(&db, b), Some(Some(false)));
        assert_eq!(db.category_overrides().unwrap().len(), 1);
    }

    #[test]
    fn a_channel_moved_into_a_category_takes_its_permissions_and_keeps_them_when_moved_out() {
        let db = Db::open(":memory:").unwrap();
        let c = db.ensure_channel("c").unwrap();
        db.set_channel_override(
            c,
            common::EVERYONE_ROLE,
            common::Setting {
                view: None,
                send: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        let cat = db.create_category("Group").unwrap().expect("made");
        db.set_category_override(
            cat,
            common::EVERYONE_ROLE,
            common::Setting {
                view: Some(false),
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        db.move_channel(c, Some(cat)).unwrap();
        let mine: Vec<common::ChannelOverride> = db
            .channel_overrides()
            .unwrap()
            .into_iter()
            .filter(|o| o.channel_id == c)
            .collect();
        assert_eq!(
            mine,
            vec![common::ChannelOverride {
                channel_id: c,
                role: common::EVERYONE_ROLE,
                view: Some(false),
                send: None,
                manage_channels: None,
                manage_messages: None
            }],
            "the channel's own setting is replaced by the category's"
        );
        db.move_channel(c, None).unwrap();
        assert_eq!(
            everyone_view(&db, c),
            Some(Some(false)),
            "leaving the category keeps the copied setting"
        );
    }

    #[test]
    fn clearing_a_category_permission_clears_it_on_its_channels() {
        let db = Db::open(":memory:").unwrap();
        let a = db.ensure_channel("a").unwrap();
        let cat = db.create_category("Group").unwrap().expect("made");
        db.move_channel(a, Some(cat)).unwrap();
        db.set_category_override(
            cat,
            common::EVERYONE_ROLE,
            common::Setting {
                view: Some(false),
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        db.set_category_override(
            cat,
            common::EVERYONE_ROLE,
            common::Setting {
                view: None,
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(db.category_overrides().unwrap().is_empty());
        assert_eq!(everyone_view(&db, a), None);
    }
}

mod ordering_tests {
    use super::super::*;

    fn names(list: &[common::ChannelRef]) -> Vec<String> {
        list.iter().map(|c| c.name.clone()).collect()
    }

    #[test]
    fn a_channel_can_be_placed_before_another_or_at_the_end_of_a_category() {
        let db = Db::open(":memory:").unwrap();
        let a = db.ensure_channel("a").unwrap();
        let b = db.ensure_channel("b").unwrap();
        let c = db.ensure_channel("c").unwrap();
        let cat = db.create_category("Group").unwrap().expect("made");
        // Move c to the front of the uncategorized list, before a.
        assert!(db.place_channel(c, None, Some(a)).unwrap());
        assert_eq!(
            names(&db.uncategorized_channels().unwrap()),
            vec!["general", "random", "c", "a", "b"]
        );
        // Put a into the category, at its end, then b before a.
        assert!(db.place_channel(a, Some(cat), None).unwrap());
        assert!(db.place_channel(b, Some(cat), Some(a)).unwrap());
        let group = db.categories().unwrap().into_iter().find(|g| g.id == cat).unwrap();
        assert_eq!(names(&group.channels), vec!["b", "a"]);
        assert_eq!(
            names(&db.uncategorized_channels().unwrap()),
            vec!["general", "random", "c"]
        );
    }

    #[test]
    fn a_category_can_be_renamed_but_not_to_a_taken_name() {
        let db = Db::open(":memory:").unwrap();
        let cat = db.create_category("Group").unwrap().expect("made");
        db.create_category("Other").unwrap().expect("made");
        assert!(!db.rename_category(cat, "Other").unwrap(), "the name is taken");
        assert!(
            !db.rename_category(9999, "Missing").unwrap(),
            "there is no such category"
        );
        assert!(db.rename_category(cat, "Renamed").unwrap());
        let names: Vec<String> = db.categories().unwrap().into_iter().map(|c| c.name).collect();
        assert!(names.contains(&"Renamed".to_string()) && !names.contains(&"Group".to_string()));
    }

    #[test]
    fn a_channel_moved_to_a_category_goes_to_its_end() {
        let db = Db::open(":memory:").unwrap();
        let x = db.ensure_channel("x").unwrap();
        let y = db.ensure_channel("y").unwrap();
        let cat = db.create_category("Group").unwrap().expect("made");
        db.move_channel(x, Some(cat)).unwrap();
        db.move_channel(y, Some(cat)).unwrap();
        let group = db.categories().unwrap().into_iter().find(|g| g.id == cat).unwrap();
        assert_eq!(names(&group.channels), vec!["x", "y"]);
    }
}

mod ban_tests {
    use super::super::*;

    #[test]
    fn bans_are_listed_with_names_and_can_be_lifted() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("bob", "Bob", 2).unwrap();
        db.ban("bob", "owner", "spamming", 100).unwrap();
        assert!(db.is_banned("bob").unwrap());
        let bans = db.bans().unwrap();
        assert_eq!(bans.len(), 1);
        assert_eq!(bans[0].user, "bob");
        assert_eq!(bans[0].name, "Bob");
        assert_eq!(bans[0].by, "Owner");
        assert_eq!(bans[0].reason, "spamming");
        assert_eq!(bans[0].at, 100);
        db.unban("bob").unwrap();
        assert!(!db.is_banned("bob").unwrap());
        assert!(db.bans().unwrap().is_empty());
    }

    #[test]
    fn a_member_removed_before_the_ban_keeps_their_name_in_the_list() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("bob", "Bob", 2).unwrap();
        db.leave_server("bob").unwrap();
        db.ban("bob", "owner", "", 5).unwrap();
        let bans = db.bans().unwrap();
        assert_eq!(bans[0].name, "Bob", "the name the account had here is kept");
    }

    #[test]
    fn a_second_ban_replaces_the_first() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("bob", "Bob", 2).unwrap();
        db.ban("bob", "owner", "first", 1).unwrap();
        db.ban("bob", "owner", "second", 2).unwrap();
        let bans = db.bans().unwrap();
        assert_eq!(bans.len(), 1);
        assert_eq!(bans[0].reason, "second");
    }
}

mod kick_tests {
    use super::super::*;

    #[test]
    fn a_kick_is_remembered_with_its_reason_until_the_account_joins_again() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("bob", "Bob", 2).unwrap();
        db.leave_server("bob").unwrap();
        db.mark_kicked("bob", "off topic").unwrap();
        assert_eq!(db.kicked_reason("bob").unwrap().as_deref(), Some("off topic"));
        db.clear_left("bob").unwrap();
        assert!(db.kicked_reason("bob").unwrap().is_none());
    }

    #[test]
    fn someone_who_left_on_their_own_was_not_kicked() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("bob", "Bob", 2).unwrap();
        db.leave_server("bob").unwrap();
        assert!(db.has_left("bob").unwrap());
        assert!(db.kicked_reason("bob").unwrap().is_none());
    }
}

mod slow_mode_tests {
    use super::super::*;

    #[test]
    fn a_channel_has_no_slow_mode_until_one_is_set_and_clearing_it_removes_it() {
        let db = Db::open(":memory:").unwrap();
        let general = db.ensure_channel("general").unwrap();
        assert_eq!(db.slow_mode(general).unwrap(), 0);
        assert!(db.slow_modes().unwrap().is_empty());

        assert!(db.set_slow_mode(general, 30).unwrap());
        assert_eq!(db.slow_mode(general).unwrap(), 30);
        assert_eq!(db.slow_modes().unwrap(), vec![common::SlowMode { channel_id: general, seconds: 30 }]);

        assert!(db.set_slow_mode(general, 0).unwrap());
        assert_eq!(db.slow_mode(general).unwrap(), 0);
        assert!(db.slow_modes().unwrap().is_empty());
    }

    #[test]
    fn slow_mode_on_a_missing_channel_is_refused() {
        let db = Db::open(":memory:").unwrap();
        assert!(!db.set_slow_mode(999, 30).unwrap());
        assert_eq!(db.slow_mode(999).unwrap(), 0);
    }

    #[test]
    fn a_database_from_before_slow_mode_gets_the_column_and_keeps_its_channels() {
        let path = std::env::temp_dir().join(format!("vaxelium-slow-mode-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE channels (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    name        TEXT NOT NULL UNIQUE,
                    category_id INTEGER,
                    position    INTEGER NOT NULL DEFAULT 0
                );
                INSERT INTO channels (name, position) VALUES ('general', 1);",
            )
            .unwrap();
        }
        let db = Db::open(path.to_str().unwrap()).unwrap();
        let general = db.channel_id("general").unwrap().unwrap();
        assert!(db.set_slow_mode(general, 60).unwrap());
        assert_eq!(db.slow_mode(general).unwrap(), 60);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
