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
            reply: None,
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
        assert!(db.is_dm_member(&dm, &new_id).unwrap(), "direct messages follow it too");
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
