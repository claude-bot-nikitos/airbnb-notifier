//! Telegram client tests against a fake Bot API server.

mod common;

use airbnb_notifier::telegram::{Telegram, Update};
use common::{FakeTelegram, MockServer, Response, TOKEN};

#[test]
fn sends_messages_with_keyboard_photos_edits_and_answers() {
    let fake = FakeTelegram::start();
    let tg = Telegram::new(&format!("{}/", fake.api_url()), TOKEN);
    let kb = vec![vec![("⏸ Pause".to_string(), "p:1".to_string())]];

    tg.send(5, "<b>hi</b>", Some(&kb)).unwrap();
    tg.send(5, "plain", None).unwrap();
    tg.send_photo(5, "https://img/x.jpg", "cap").unwrap();
    tg.edit(5, 77, "edited", Some(&kb)).unwrap();
    tg.edit(5, 78, "edited2", None).unwrap();
    tg.answer_callback("cb1", "Paused").unwrap();
    tg.set_commands().unwrap();

    let calls = fake.calls();
    let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        methods,
        [
            "sendMessage",
            "sendMessage",
            "sendPhoto",
            "editMessageText",
            "editMessageText",
            "answerCallbackQuery",
            "setMyCommands"
        ]
    );
    let msg = &calls[0].1;
    assert_eq!(msg["chat_id"], 5);
    assert_eq!(msg["parse_mode"], "HTML");
    assert_eq!(msg["link_preview_options"]["is_disabled"], true);
    assert_eq!(
        msg["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
        "p:1"
    );
    assert!(calls[1].1.get("reply_markup").is_none());
    assert_eq!(calls[2].1["photo"], "https://img/x.jpg");
    assert_eq!(calls[2].1["caption"], "cap");
    assert_eq!(calls[3].1["message_id"], 77);
    assert_eq!(calls[5].1["callback_query_id"], "cb1");
    let commands: Vec<&str> = calls[6].1["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["command"].as_str().unwrap())
        .collect();
    assert_eq!(commands, ["list", "check", "help"]);
}

#[test]
fn receives_updates_and_advances_offset() {
    let fake = FakeTelegram::start();
    let tg = Telegram::new(&fake.api_url(), TOKEN);
    fake.user_says(7, "/start");
    fake.user_taps(7, "p:3");

    let (next, updates) = tg.get_updates(0, 1).unwrap();
    assert_eq!(next, 3);
    assert_eq!(
        updates,
        vec![
            Update::Message {
                chat_id: 7,
                user_id: 7,
                text: "/start".into()
            },
            Update::Callback {
                id: "cbp:3".into(),
                chat_id: 7,
                user_id: 7,
                message_id: 77,
                data: "p:3".into()
            },
        ]
    );
    let req = fake.server.requests().pop().unwrap();
    assert_eq!(
        req.json()["allowed_updates"],
        serde_json::json!(["message", "callback_query"])
    );

    // Acknowledged updates are not delivered again.
    let (next2, again) = tg.get_updates(next, 1).unwrap();
    assert_eq!(next2, 3);
    assert!(again.is_empty());
}

#[test]
fn api_errors_include_telegram_description_but_never_the_token() {
    let server = MockServer::start(|_| {
        Response::status(
            400,
            r#"{"ok":false,"description":"Bad Request: chat not found"}"#,
        )
    });
    let tg = Telegram::new(&server.url(), TOKEN);
    let err = tg.send(1, "x", None).unwrap_err().to_string();
    assert!(err.contains("HTTP 400"), "{err}");
    assert!(err.contains("chat not found"), "{err}");
    assert!(!err.contains(TOKEN), "{err}");
}

#[test]
fn ok_false_with_200_is_an_error() {
    let server = MockServer::start(|_| {
        Response::json(serde_json::json!({"ok": false, "description": "nope"}))
    });
    let tg = Telegram::new(&server.url(), TOKEN);
    assert!(tg.answer_callback("x", "y").is_err());
}

#[test]
fn transport_errors_never_include_the_token() {
    let tg = Telegram::new("http://127.0.0.1:1", TOKEN);
    let err = tg.get_updates(0, 1).unwrap_err().to_string();
    assert!(err.contains("transport error"), "{err}");
    assert!(!err.contains(TOKEN), "{err}");
}
