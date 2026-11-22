mod role_tests {
    use super::super::*;

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
        let mods = db
            .create_role("Mods", "#00ff00", perm::MANAGE_MESSAGES, false, false)
            .unwrap()
            .unwrap();
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
        let readers = db
            .create_role("Readers", "", perm::VIEW_CHANNEL | perm::SEND_MESSAGES, false, false)
            .unwrap()
            .unwrap();
        let viewers = db
            .create_role("Viewers", "", perm::VIEW_CHANNEL, false, false)
            .unwrap()
            .unwrap();
        db.set_member_role("bob", readers, true).unwrap();
        db.set_member_role("carol", viewers, true).unwrap();
        // Take the channel permissions off @everyone. Other roles hold them, so that is allowed.
        let off = apply_role_change(
            &db,
            "owner",
            RoleChange::Update {
                role: EVERYONE_ROLE,
                name: String::new(),
                color: String::new(),
                permissions: 0,
                mentionable: false,
                hoisted: false,
            },
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
        db.set_channel_override(
            general,
            viewers,
            common::Setting {
                view: Some(false),
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!Access::new(&db, "carol").unwrap().allows(general, perm::VIEW_CHANNEL));

        // An allowance on the channel gives a role access it does not have server-wide.
        db.set_channel_override(
            general,
            viewers,
            common::Setting {
                view: None,
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        db.set_channel_override(
            general,
            EVERYONE_ROLE,
            common::Setting {
                view: Some(true),
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(Access::new(&db, "erin").unwrap().allows(general, perm::VIEW_CHANNEL));

        // A send denial stops sending but not viewing.
        db.set_channel_override(
            general,
            viewers,
            common::Setting {
                view: None,
                send: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!Access::new(&db, "carol").unwrap().allows(general, perm::SEND_MESSAGES));
        assert!(Access::new(&db, "carol").unwrap().allows(general, perm::VIEW_CHANNEL));

        // Removing an exception puts the server-wide permission back.
        db.set_channel_override(
            general,
            viewers,
            common::Setting {
                view: None,
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!Access::new(&db, "carol").unwrap().allows(general, perm::SEND_MESSAGES));
        assert!(db.channel_overrides().unwrap().iter().all(|o| o.role != viewers));
    }

    #[test]
    fn ping_and_member_list_flags_are_saved_and_changed() {
        let (db, _, _) = server();
        let made = apply_role_change(
            &db,
            "owner",
            RoleChange::Create {
                name: "Pingers".into(),
                color: String::new(),
                permissions: 0,
                mentionable: true,
                hoisted: true,
            },
        )
        .unwrap();
        assert!(made.is_ok());
        let pingers = db
            .role_list()
            .unwrap()
            .into_iter()
            .find(|r| r.name == "Pingers")
            .unwrap();
        assert!(pingers.mentionable && pingers.hoisted);
        let off = apply_role_change(
            &db,
            "owner",
            RoleChange::Update {
                role: pingers.id,
                name: "Pingers".into(),
                color: String::new(),
                permissions: 0,
                mentionable: false,
                hoisted: false,
            },
        )
        .unwrap();
        assert!(off.is_ok());
        let pingers = db
            .role_list()
            .unwrap()
            .into_iter()
            .find(|r| r.id == pingers.id)
            .unwrap();
        assert!(!pingers.mentionable && !pingers.hoisted);
    }

    #[test]
    fn owner_can_do_everything() {
        let (db, _, _) = server();
        let made = apply_role_change(
            &db,
            "owner",
            RoleChange::Create {
                name: "Helpers".into(),
                color: "#123456".into(),
                permissions: perm::ALL,
                mentionable: false,
                hoisted: false,
            },
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
            RoleChange::Member {
                user: "carol".into(),
                role: mods,
                on: true,
            },
        )
        .unwrap();
        assert!(refused.is_err());
        let refused = apply_role_change(
            &db,
            "carol",
            RoleChange::Create {
                name: "X".into(),
                color: String::new(),
                permissions: 0,
                mentionable: false,
                hoisted: false,
            },
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
            RoleChange::Member {
                user: "carol".into(),
                role: mods,
                on: true,
            },
        )
        .unwrap();
        assert_eq!(given, Ok(()));
        assert_eq!(db.member_role_ids("carol").unwrap(), vec![mods]);

        // Admin is not below bob's own highest role, so bob cannot give it.
        let refused = apply_role_change(
            &db,
            "bob",
            RoleChange::Member {
                user: "carol".into(),
                role: admin,
                on: true,
            },
        )
        .unwrap();
        assert!(refused.is_err());
        // Dan has the same highest role as bob, so bob cannot change his roles.
        let refused = apply_role_change(
            &db,
            "bob",
            RoleChange::Member {
                user: "dan".into(),
                role: mods,
                on: false,
            },
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
            RoleChange::Create {
                name: "Owners".into(),
                color: String::new(),
                permissions: perm::ALL,
                mentionable: false,
                hoisted: false,
            },
        )
        .unwrap();
        // Bob has MANAGE_ROLES and MANAGE_CHANNELS, not MANAGE_MESSAGES.
        assert!(refused.is_err());
        let fine = apply_role_change(
            &db,
            "bob",
            RoleChange::Create {
                name: "Cleaners".into(),
                color: String::new(),
                permissions: perm::MANAGE_CHANNELS,
                mentionable: false,
                hoisted: false,
            },
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
            RoleChange::Create {
                name: "Mods".into(),
                color: String::new(),
                permissions: 0,
                mentionable: false,
                hoisted: false,
            },
        )
        .unwrap();
        assert!(taken.is_err());
        let bad_colour = apply_role_change(
            &db,
            "owner",
            RoleChange::Update {
                role: mods,
                name: "Mods".into(),
                color: "red".into(),
                permissions: 0,
                mentionable: false,
                hoisted: false,
            },
        )
        .unwrap();
        assert!(bad_colour.is_err());
    }

    #[test]
    fn deleting_a_role_takes_it_from_members() {
        let (db, _, mods) = server();
        apply_role_change(
            &db,
            "owner",
            RoleChange::Member {
                user: "carol".into(),
                role: mods,
                on: true,
            },
        )
        .unwrap()
        .unwrap();
        apply_role_change(&db, "owner", RoleChange::Delete { role: mods })
            .unwrap()
            .unwrap();
        assert!(db.member_role_ids("carol").unwrap().is_empty());
        assert!(db.role_list().unwrap().iter().all(|r| r.id != mods));
    }

    #[test]
    fn admins_cannot_move_roles_above_their_own() {
        let (db, admin, mods) = server();
        // Bob tries to move Mods above Admin, which would put it above his own role.
        let refused = apply_role_change(
            &db,
            "bob",
            RoleChange::Move {
                role: mods,
                before: Some(admin),
            },
        )
        .unwrap();
        assert!(refused.is_err());
        // Bob cannot move Admin at all: it is his own highest role.
        let refused = apply_role_change(
            &db,
            "bob",
            RoleChange::Move {
                role: admin,
                before: None,
            },
        )
        .unwrap();
        assert!(refused.is_err());
        // The owner can.
        let moved = apply_role_change(
            &db,
            "owner",
            RoleChange::Move {
                role: admin,
                before: None,
            },
        )
        .unwrap();
        assert_eq!(moved, Ok(()));
        let order: Vec<i64> = db
            .role_list()
            .unwrap()
            .iter()
            .map(|r| r.id)
            .filter(|&id| id != EVERYONE_ROLE)
            .collect();
        assert_eq!(order, vec![mods, admin]);
    }

    #[test]
    fn only_the_owner_changes_the_owners_roles() {
        let (db, admin, mods) = server();
        // Bob outranks Mods, but the owner outranks everyone, so he may not change the owner's roles.
        let refused = apply_role_change(
            &db,
            "bob",
            RoleChange::Member {
                user: "owner".into(),
                role: mods,
                on: true,
            },
        )
        .unwrap();
        assert!(refused.is_err());
        // The owner may give themselves a role, and it shows on the owner.
        let given = apply_role_change(
            &db,
            "owner",
            RoleChange::Member {
                user: "owner".into(),
                role: admin,
                on: true,
            },
        )
        .unwrap();
        assert_eq!(given, Ok(()));
        assert_eq!(db.member_role_ids("owner").unwrap(), vec![admin]);
    }
}

mod member_override_tests {
    use super::super::*;

    fn server() -> Db {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("carol", "Carol", 2).unwrap();
        db
    }

    #[test]
    fn a_members_setting_beats_their_roles_setting_for_that_channel() {
        let db = server();
        let general = db.ensure_channel("general").unwrap();
        // Everyone may view the channel, but carol is denied it herself.
        db.set_channel_override(
            general,
            EVERYONE_ROLE,
            common::Setting {
                view: Some(true),
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(Access::new(&db, "carol").unwrap().allows(general, perm::VIEW_CHANNEL));
        db.member_channel_override(
            general,
            "carol",
            common::Setting {
                view: Some(false),
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !Access::new(&db, "carol").unwrap().allows(general, perm::VIEW_CHANNEL),
            "carol's own denial wins"
        );
        assert!(
            Access::new(&db, "owner").unwrap().allows(general, perm::VIEW_CHANNEL),
            "other members are not affected"
        );
        // A member's allowance wins over a role's denial too.
        db.set_channel_override(
            general,
            EVERYONE_ROLE,
            common::Setting {
                view: Some(false),
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        db.member_channel_override(
            general,
            "carol",
            common::Setting {
                view: Some(true),
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(Access::new(&db, "carol").unwrap().allows(general, perm::VIEW_CHANNEL));
    }

    #[test]
    fn manage_channels_can_be_given_or_taken_per_channel() {
        let db = server();
        let a = db.ensure_channel("a").unwrap();
        let b = db.ensure_channel("b").unwrap();
        // A role with Manage channels everywhere, which carol holds.
        let mods = db
            .create_role("Mods", "", perm::MANAGE_CHANNELS, false, false)
            .unwrap()
            .unwrap();
        apply_role_change(
            &db,
            "owner",
            RoleChange::Member {
                user: "carol".into(),
                role: mods,
                on: true,
            },
        )
        .unwrap()
        .unwrap();
        assert!(Access::new(&db, "carol").unwrap().manages(a, perm::MANAGE_CHANNELS));
        // Taken away in channel a only.
        db.set_channel_override(
            a,
            mods,
            common::Setting {
                manage_channels: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !Access::new(&db, "carol").unwrap().manages(a, perm::MANAGE_CHANNELS),
            "the channel's denial wins"
        );
        assert!(
            Access::new(&db, "carol").unwrap().manages(b, perm::MANAGE_CHANNELS),
            "other channels keep it"
        );
        // Manage messages is separate from manage channels.
        assert!(!Access::new(&db, "carol").unwrap().manages(b, perm::MANAGE_MESSAGES));
    }

    #[test]
    fn a_category_manage_setting_reaches_its_channels() {
        let db = server();
        let a = db.ensure_channel("a").unwrap();
        let cat = db.create_category("Group").unwrap().expect("made");
        db.move_channel(a, Some(cat)).unwrap();
        let mods = db.create_role("Mods", "", 0, false, false).unwrap().unwrap();
        apply_role_change(
            &db,
            "owner",
            RoleChange::Member {
                user: "carol".into(),
                role: mods,
                on: true,
            },
        )
        .unwrap()
        .unwrap();
        assert!(!Access::new(&db, "carol")
            .unwrap()
            .manages_category(cat, perm::MANAGE_CHANNELS));
        db.set_category_override(
            cat,
            mods,
            common::Setting {
                manage_channels: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(Access::new(&db, "carol")
            .unwrap()
            .manages_category(cat, perm::MANAGE_CHANNELS));
        assert!(
            Access::new(&db, "carol").unwrap().manages(a, perm::MANAGE_CHANNELS),
            "the channel in it is managed too"
        );
    }

    #[test]
    fn a_category_member_setting_reaches_its_channels_and_moved_in_channels() {
        let db = server();
        let a = db.ensure_channel("a").unwrap();
        let cat = db.create_category("Group").unwrap().expect("made");
        db.move_channel(a, Some(cat)).unwrap();
        db.member_category_override(
            cat,
            "carol",
            common::Setting {
                view: Some(false),
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!Access::new(&db, "carol").unwrap().allows(a, perm::VIEW_CHANNEL));
        // A channel moved into the category takes the member settings too.
        let b = db.ensure_channel("b").unwrap();
        db.move_channel(b, Some(cat)).unwrap();
        assert!(!Access::new(&db, "carol").unwrap().allows(b, perm::VIEW_CHANNEL));
        assert_eq!(db.category_member_overrides().unwrap().len(), 1);
    }
}

mod everyone_tests {
    use super::super::*;

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
        assert_eq!(
            db.role_list().unwrap().last().map(|r| r.id),
            Some(EVERYONE_ROLE),
            "new roles go above @everyone"
        );
        assert!(
            apply_role_change(&db, "owner", RoleChange::Delete { role: EVERYONE_ROLE })
                .unwrap()
                .is_err()
        );
        assert!(apply_role_change(
            &db,
            "owner",
            RoleChange::Move {
                role: EVERYONE_ROLE,
                before: None
            }
        )
        .unwrap()
        .is_err());
        assert!(apply_role_change(
            &db,
            "owner",
            RoleChange::Member {
                user: "carol".into(),
                role: EVERYONE_ROLE,
                on: true
            }
        )
        .unwrap()
        .is_err());
        // Reordering the real roles leaves @everyone at the bottom.
        apply_role_change(
            &db,
            "owner",
            RoleChange::Move {
                role: mods,
                before: None,
            },
        )
        .unwrap()
        .unwrap();
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
        let everyone = db
            .role_list()
            .unwrap()
            .into_iter()
            .find(|r| r.id == EVERYONE_ROLE)
            .unwrap();
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
        db.set_channel_override(
            general,
            EVERYONE_ROLE,
            common::Setting {
                view: None,
                send: None,
                ..Default::default()
            },
        )
        .unwrap();
        let admins = db
            .create_role("Administrators", "", perm::ADMINISTRATOR, false, false)
            .unwrap()
            .unwrap();
        db.set_member_role("erin", admins, true).unwrap();
        db.set_channel_override(
            general,
            admins,
            common::Setting {
                view: Some(false),
                send: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
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
    fn status_is_saved_per_member() {
        let db = server();
        assert!(db.statuses().unwrap().is_empty());
        db.set_status("bob", common::PresenceStatus::DoNotDisturb).unwrap();
        db.set_status("bob", common::PresenceStatus::Idle).unwrap();
        db.set_status("carol", common::PresenceStatus::Invisible).unwrap();
        let statuses = db.statuses().unwrap();
        assert_eq!(statuses.get("bob"), Some(&common::PresenceStatus::Idle));
        assert_eq!(statuses.get("carol"), Some(&common::PresenceStatus::Invisible));
        assert!(!statuses.contains_key("dan"));
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
        let start = db
            .role_list()
            .unwrap()
            .into_iter()
            .find(|r| r.id == EVERYONE_ROLE)
            .unwrap();
        assert_eq!(
            start.permissions & (perm::VIEW_CHANNEL | perm::SEND_MESSAGES),
            perm::VIEW_CHANNEL | perm::SEND_MESSAGES
        );

        // Nobody else holds them yet, so they cannot be taken away.
        assert_eq!(
            apply_role_change(&db, "owner", everyone(0)).unwrap(),
            Err(EVERYONE_KEEPS.into())
        );
        assert_eq!(
            apply_role_change(&db, "owner", everyone(perm::VIEW_CHANNEL)).unwrap(),
            Err(EVERYONE_KEEPS.into())
        );

        // Once another role holds View channels, @everyone may drop it, but not Message channels.
        db.create_role("Viewers", "", perm::VIEW_CHANNEL, false, false)
            .unwrap()
            .unwrap();
        assert_eq!(
            apply_role_change(&db, "owner", everyone(perm::SEND_MESSAGES)).unwrap(),
            Ok(())
        );
        assert_eq!(
            apply_role_change(&db, "owner", everyone(0)).unwrap(),
            Err(EVERYONE_KEEPS.into())
        );
    }
}

mod badge_tests {
    use super::super::*;

    #[test]
    fn badges_are_unique_by_name_and_deleting_one_takes_it_from_members() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("bob", "Bob", 1).unwrap();
        let helper = db.create_badge("Helper", "#2ecc71").unwrap().expect("made");
        assert!(
            db.create_badge("Helper", "#000000").unwrap().is_none(),
            "the name is taken"
        );
        db.set_member_badge("bob", helper, true).unwrap();
        assert_eq!(db.all_member_badges().unwrap().get("bob"), Some(&vec![helper]));
        assert!(db.delete_badge(helper).unwrap());
        assert!(db.badge_list().unwrap().is_empty());
        assert!(db.all_member_badges().unwrap().get("bob").is_none());
    }
}

mod channel_name_tests {
    use super::super::*;

    #[test]
    fn channel_names_keep_emoji_and_symbols() {
        assert_eq!(normalize_channel("#General"), "general");
        assert_eq!(normalize_channel("  dev-ops_1  "), "dev-ops_1");
        assert_eq!(normalize_channel("🎮games"), "🎮games");
        assert_eq!(normalize_channel("café ☕ chat"), "café☕chat");
        assert_eq!(normalize_channel("a\tb\nc"), "abc", "whitespace and control characters are dropped");
        assert_eq!(normalize_channel("   "), "", "a name with nothing left is empty");
        assert_eq!(normalize_channel(&"🎮".repeat(40)).chars().count(), 32, "at most 32 characters");
    }
}

#[cfg(test)]
mod emoji_image_tests {
    use crate::emoji_image::{check, MAX_BYTES, MAX_SIZE};

    const STILL: &[u8] = include_bytes!("fixtures/emoji-still.png");
    const ANIMATED: &[u8] = include_bytes!("fixtures/emoji-animated.png");

    #[test]
    fn png_and_animated_png_are_accepted() {
        assert_eq!(check(STILL), Ok(()));
        assert_eq!(check(ANIMATED), Ok(()));
    }

    #[test]
    fn other_formats_are_refused_whatever_their_name() {
        // A GIF, a JPEG and a WebP, by their signatures. The name does not matter.
        assert!(check(b"GIF89a\x10\x00\x10\x00").is_err());
        assert!(check(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F']).is_err());
        assert!(check(b"RIFF\x24\x00\x00\x00WEBPVP8 ").is_err());
    }

    #[test]
    fn a_png_that_is_really_something_else_is_refused() {
        // The signature is right, but the rest is not a PNG.
        let mut fake = STILL[..8].to_vec();
        fake.extend_from_slice(b"not really a png at all");
        assert!(check(&fake).is_err());
    }

    #[test]
    fn size_limits_are_enforced() {
        let big = vec![0u8; MAX_BYTES + 1];
        assert!(check(&big).unwrap_err().contains("KB"), "too many bytes");
        // Dimensions are read from the header: a 16 by 16 picture is fine, a wider header is not.
        let mut wide = STILL.to_vec();
        // IHDR's width is the four bytes after the chunk type, which starts at byte 12.
        wide[16..20].copy_from_slice(&(MAX_SIZE + 1).to_be_bytes());
        assert!(check(&wide).is_err());
    }

    #[test]
    fn emoji_names_are_short_lowercase_words() {
        use crate::valid_emoji_name;
        assert!(valid_emoji_name("party"));
        assert!(valid_emoji_name("cat_2"));
        assert!(!valid_emoji_name("a"), "too short");
        assert!(!valid_emoji_name("Party"), "capitals are not allowed");
        assert!(!valid_emoji_name("hi there"), "spaces are not allowed");
        assert!(!valid_emoji_name("🎉party"), "only letters, digits and underscores");
        assert!(!valid_emoji_name(&"a".repeat(33)), "too long");
    }
}

#[cfg(test)]
mod emoji_change_tests {
    use crate::*;
    use std::sync::{Arc, Mutex};

    const STILL: &[u8] = include_bytes!("fixtures/emoji-still.png");

    fn state_with_owner() -> SharedState {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("carol", "Carol", 2).unwrap();
        db.transfer_ownership("owner").unwrap();
        Arc::new(Mutex::new(State {
            db,
            online: Vec::new(),
            advertise: String::new(),
            peer_failures: HashMap::new(),
            dead_peers: HashMap::new(),
            name_alerts: std::collections::HashSet::new(),
            mention_log: HashMap::new(),
            timeouts: HashMap::new(),
            slow_last: HashMap::new(),
        }))
    }

    fn encoded(bytes: &[u8]) -> String {
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
    }

    #[test]
    fn the_owner_adds_an_emoji_and_everyone_is_sent_the_list() {
        let state = state_with_owner();
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let refused = change_emoji(&state, &tx, "owner", EmojiChange::Add { name: "Party".into(), image: encoded(STILL) }).unwrap();
        assert_eq!(refused, None, "a PNG from the owner is added");
        assert_eq!(state.lock().unwrap().db.emoji_count().unwrap(), 1);
        match rx.try_recv() {
            Ok(ServerMsg::CustomEmoji { emoji }) => assert_eq!(emoji[0].name, "party"),
            other => panic!("everyone is sent the list: {other:?}"),
        }
    }

    #[test]
    fn a_member_without_the_permission_is_refused() {
        let state = state_with_owner();
        let (tx, _rx) = tokio::sync::broadcast::channel(8);
        let refused = change_emoji(&state, &tx, "carol", EmojiChange::Add { name: "party".into(), image: encoded(STILL) }).unwrap();
        assert!(refused.unwrap().contains("Manage Emoji"));
    }

    #[test]
    fn a_picture_that_is_not_base64_is_refused_with_a_reason() {
        let state = state_with_owner();
        let (tx, _rx) = tokio::sync::broadcast::channel(8);
        let refused = change_emoji(&state, &tx, "owner", EmojiChange::Add { name: "party".into(), image: "not base64!".into() }).unwrap();
        assert_eq!(refused.as_deref(), Some("that image could not be read"));
    }
}

mod snapshot_tests {
    use crate::*;
    use std::sync::{Arc, Mutex};

    const STILL: &[u8] = include_bytes!("fixtures/emoji-still.png");

    fn state() -> SharedState {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.transfer_ownership("owner").unwrap();
        Arc::new(Mutex::new(State {
            db,
            online: Vec::new(),
            advertise: String::new(),
            peer_failures: HashMap::new(),
            dead_peers: HashMap::new(),
            name_alerts: std::collections::HashSet::new(),
            mention_log: HashMap::new(),
            timeouts: HashMap::new(),
            slow_last: HashMap::new(),
        }))
    }

    /// The messages a snapshot sends, in order, as they would be read by a client.
    async fn snapshot(state: &SharedState) -> Vec<ServerMsg> {
        let mut out: Vec<u8> = Vec::new();
        send_snapshot(&mut out, state).await.unwrap();
        String::from_utf8(out).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }

    #[tokio::test]
    async fn the_snapshot_sends_each_server_wide_list_once_and_in_order() {
        let msgs = snapshot(&state()).await;
        let kinds: Vec<&str> = msgs
            .iter()
            .map(|m| match m {
                ServerMsg::Roles { .. } => "roles",
                ServerMsg::Badges { .. } => "badges",
                ServerMsg::CustomEmoji { .. } => "emoji",
                ServerMsg::ChannelOverrides { .. } => "channel overrides",
                ServerMsg::SlowModes { .. } => "slow modes",
                ServerMsg::CategoryOverrides { .. } => "category overrides",
                ServerMsg::MemberOverrides { .. } => "member overrides",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            ["roles", "badges", "emoji", "channel overrides", "slow modes", "category overrides", "member overrides"]
        );
    }

    #[tokio::test]
    async fn a_resent_snapshot_includes_a_change_that_was_missed() {
        let state = state();
        let (tx, _rx) = tokio::sync::broadcast::channel(8);
        // The emoji was added while the connection was not reading its events.
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, STILL);
        change_emoji(&state, &tx, "owner", EmojiChange::Add { name: "party".into(), image: encoded }).unwrap();
        let emoji = snapshot(&state).await.into_iter().find_map(|m| match m {
            ServerMsg::CustomEmoji { emoji } => Some(emoji),
            _ => None,
        });
        assert_eq!(emoji.unwrap()[0].name, "party");
    }
}

mod schedule_tests {
    use crate::*;
    use std::sync::{Arc, Mutex};

    fn state() -> SharedState {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("carol", "Carol", 2).unwrap();
        db.transfer_ownership("owner").unwrap();
        db.ensure_channel("general").unwrap();
        Arc::new(Mutex::new(State {
            db,
            online: Vec::new(),
            advertise: String::new(),
            peer_failures: HashMap::new(),
            dead_peers: HashMap::new(),
            name_alerts: std::collections::HashSet::new(),
            mention_log: HashMap::new(),
            timeouts: HashMap::new(),
            slow_last: HashMap::new(),
        }))
    }

    #[test]
    fn a_scheduled_message_is_due_only_once_its_time_has_come() {
        let db = Db::open(":memory:").unwrap();
        let channel = db.ensure_channel("general").unwrap();
        db.schedule_message("owner", channel, "later", 1000).unwrap();
        assert!(db.due_scheduled(999).unwrap().is_empty());
        let due = db.due_scheduled(1000).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].3, "later");
    }

    #[test]
    fn members_see_and_cancel_only_their_own_scheduled_messages() {
        let db = Db::open(":memory:").unwrap();
        let channel = db.ensure_channel("general").unwrap();
        let mine = db.schedule_message("owner", channel, "mine", 1000).unwrap();
        db.schedule_message("carol", channel, "hers", 1000).unwrap();
        assert_eq!(db.scheduled_for("owner").unwrap().len(), 1);
        assert!(!db.cancel_scheduled("carol", mine).unwrap(), "carol cannot cancel owner's message");
        assert!(db.cancel_scheduled("owner", mine).unwrap());
        assert!(db.scheduled_for("owner").unwrap().is_empty());
    }

    #[test]
    fn a_due_message_is_posted_as_its_author_and_taken_off_the_list() {
        let state = state();
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let channel = state.lock().unwrap().db.ensure_channel("general").unwrap();
        let id = state.lock().unwrap().db.schedule_message("owner", channel, "good morning", 1000).unwrap();
        post_scheduled(&state, &tx, &Arc::new(String::new()), id, "owner", channel, "good morning".into()).unwrap();
        match rx.try_recv() {
            Ok(ServerMsg::Message { message }) => {
                assert_eq!(message.text, "good morning");
                assert_eq!(message.author_id, "owner");
            }
            other => panic!("the message is posted: {other:?}"),
        }
        assert!(state.lock().unwrap().db.scheduled_for("owner").unwrap().is_empty());
    }
}

mod schedule_permission_tests {
    use crate::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_due_message_is_not_posted_once_its_author_lacks_manage_messages() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("carol", "Carol", 2).unwrap();
        db.transfer_ownership("owner").unwrap();
        let channel = db.ensure_channel("general").unwrap();
        let id = db.schedule_message("carol", channel, "sneaky", 1000).unwrap();
        let state: SharedState = Arc::new(Mutex::new(State {
            db,
            online: Vec::new(),
            advertise: String::new(),
            peer_failures: HashMap::new(),
            dead_peers: HashMap::new(),
            name_alerts: std::collections::HashSet::new(),
            mention_log: HashMap::new(),
            timeouts: HashMap::new(),
            slow_last: HashMap::new(),
        }));
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        post_scheduled(&state, &tx, &Arc::new(String::new()), id, "carol", channel, "sneaky".into()).unwrap();
        assert!(rx.try_recv().is_err(), "nothing is posted for a member without Manage Messages");
        assert!(state.lock().unwrap().db.scheduled_for("carol").unwrap().is_empty(), "and it is dropped from the list");
    }
}

mod schedule_edit_tests {
    use crate::*;

    #[test]
    fn a_scheduled_message_can_be_changed_by_its_author_only_and_keeps_its_channel() {
        let db = Db::open(":memory:").unwrap();
        db.add_member("owner", "Owner", 1).unwrap();
        db.add_member("carol", "Carol", 2).unwrap();
        let channel = db.ensure_channel("general").unwrap();
        let id = db.schedule_message("owner", channel, "draft", 1000).unwrap();
        assert_eq!(db.scheduled_channel("owner", id).unwrap(), Some(channel));
        assert_eq!(db.scheduled_channel("carol", id).unwrap(), None, "carol cannot see it");
        assert!(!db.update_scheduled("carol", id, "mine now", 2000).unwrap(), "carol cannot change it");
        assert!(db.update_scheduled("owner", id, "final", 2000).unwrap());
        let kept = db.scheduled_for("owner").unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!((kept[0].text.as_str(), kept[0].send_at, kept[0].channel_id), ("final", 2000, channel));
    }
}
