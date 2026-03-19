use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use agent_client_protocol as acp;

use super::{forwarded_session_update, latest_registry_binary};

#[test]
fn forwarded_session_update_filters_client_side_noise() {
    assert!(
        forwarded_session_update(acp::SessionUpdate::UserMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::from("prompt"))
        ))
        .is_none()
    );
    assert!(
        forwarded_session_update(acp::SessionUpdate::CurrentModeUpdate(
            acp::CurrentModeUpdate::new("auto")
        ))
        .is_none()
    );
    assert!(
        forwarded_session_update(acp::SessionUpdate::AvailableCommandsUpdate(
            acp::AvailableCommandsUpdate::new(Vec::new())
        ))
        .is_none()
    );
}

#[test]
fn forwarded_session_update_keeps_agent_output() {
    let update = acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
        acp::ContentBlock::from("answer"),
    ));

    assert!(matches!(
        forwarded_session_update(update),
        Some(acp::SessionUpdate::AgentMessageChunk(_))
    ));
}

#[test]
fn latest_registry_binary_prefers_last_installed_version() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("codex-companion-registry-{unique}"));
    let older = root.join("v_a");
    let newer = root.join("v_b");
    let binary_name = if cfg!(windows) {
        "codex-acp.exe"
    } else {
        "codex-acp"
    };

    fs::create_dir_all(&older).expect("older registry entry should exist");
    fs::create_dir_all(&newer).expect("newer registry entry should exist");
    fs::write(older.join(binary_name), []).expect("older binary should exist");
    fs::write(newer.join(binary_name), []).expect("newer binary should exist");

    assert_eq!(latest_registry_binary(&root), Some(newer.join(binary_name)));

    let _ = fs::remove_dir_all(root);
}
