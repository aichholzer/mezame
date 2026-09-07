//! The four faults that share one outcome: the frame is discarded, no
//! event is emitted, no Backend method is invoked, and the attach stays
//! open.
//!
//! The four are indistinguishable to a client on purpose. An `error` frame
//! would raise a notice for something the user never composed, and closing
//! the attach would evict a browser over one bad frame during a version
//! skew. What each case proves is that nothing reached the hub and that
//! the loop is still running afterwards, shown by a well-formed `cancel`
//! getting through straight after the bad frame.
//!
//! `parse_browser_command` is private, so the arms are driven through the
//! public `run_attach_loop`. No Backend is involved: the loop needs a
//! command sender, a broadcast receiver and a frame stream, and this file
//! supplies all three itself.
//!
//! The same harness covers the loop's exit arms that need no Backend: the
//! inbound stream ending, the hub's inbox going away, the broadcast
//! channel closing, and a lagged receiver skipping ahead rather than
//! ending. A regression that turned one of those `break`s into a
//! `continue` shipped once before (0.8.7), and nothing asserted them.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::Message;
use futures_util::Stream;
use mezame::hub::HubCommand;
use mezame::ws::run_attach_loop;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const ATTACH_ID: u64 = 7;

fn channel_stream(
    rx: mpsc::UnboundedReceiver<Message>,
) -> impl Stream<Item = Result<Message, Infallible>> + Unpin {
    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|m| (Ok(m), rx))
    }))
}

/// A running attach loop with its inbound frame channel, its command
/// receiver, its broadcast sender and its sink. Every field is held for
/// the length of a test unless the case is about dropping one: the loop
/// ends when the browser channel closes, when the command receiver goes,
/// or when the broadcast sender goes.
struct Harness {
    browser: mpsc::UnboundedSender<Message>,
    commands: mpsc::Receiver<HubCommand>,
    outbound: broadcast::Sender<Arc<Value>>,
    /// What the loop writes to the browser. Held so the bounded queue
    /// stays open; the exit-arm cases read it.
    sink: mpsc::Receiver<Message>,
    handle: JoinHandle<()>,
}

fn harness() -> Harness {
    let (cmd_tx, cmd_rx) = mpsc::channel::<HubCommand>(8);
    let (out_tx, out_rx) = broadcast::channel::<Arc<Value>>(16);
    let (browser_tx, browser_rx) = mpsc::unbounded_channel::<Message>();
    let (to_ws_tx, to_ws_rx) = mpsc::channel::<Message>(32);
    let handle = tokio::spawn(async move {
        let mut stream = channel_stream(browser_rx);
        // A long heartbeat: nothing here should be evicted mid-test.
        run_attach_loop(
            &mut stream,
            &to_ws_tx,
            out_rx,
            cmd_tx,
            ATTACH_ID,
            Duration::from_secs(30),
            Duration::from_secs(300),
        )
        .await;
    });
    Harness {
        browser: browser_tx,
        commands: cmd_rx,
        outbound: out_tx,
        sink: to_ws_rx,
        handle,
    }
}

impl Harness {
    fn send_text(&self, text: impl Into<String>) {
        self.browser
            .send(Message::Text(text.into()))
            .expect("the loop is still reading");
    }

    fn send_json(&self, value: Value) {
        self.send_text(value.to_string());
    }

    /// Assert that no command reaches the hub in the next 150ms.
    async fn expect_nothing_forwarded(&mut self) {
        tokio::time::sleep(Duration::from_millis(150)).await;
        match self.commands.try_recv() {
            Err(mpsc::error::TryRecvError::Empty) => {}
            other => panic!("a discarded frame reached the hub: {other:?}"),
        }
    }

    /// Prove the attach is still open: a well-formed `cancel` gets
    /// through.
    async fn expect_still_running(&mut self) {
        self.send_json(json!({ "type": "cancel" }));
        let forwarded = timeout(Duration::from_secs(2), self.commands.recv())
            .await
            .expect("the loop forwards within 2s")
            .expect("the command channel is open");
        assert!(
            matches!(forwarded, HubCommand::Cancel),
            "expected the cancel to come through, got {forwarded:?}"
        );
        assert!(
            !self.handle.is_finished(),
            "the attach loop exited on a discarded frame"
        );
    }
}

#[tokio::test]
async fn a_text_frame_that_is_not_json_is_discarded() {
    // Requirement 11 criterion 8's first clause. This one is rejected in
    // `run_attach_loop` itself, before the command parser runs.
    let mut h = harness();
    h.send_text("not json at all");
    h.send_text("{ unbalanced");
    h.send_text("");
    h.expect_nothing_forwarded().await;
    h.expect_still_running().await;
}

#[tokio::test]
async fn a_frame_that_is_not_an_object_or_names_no_command_is_discarded() {
    // The rest of Requirement 11 criterion 8: valid JSON that is not an
    // object, an absent `type`, a `type` that is not a string, and a
    // `type` naming no member of the command set.
    let mut h = harness();
    h.send_text("[1, 2, 3]");
    h.send_text("\"just a string\"");
    h.send_json(json!({ "blocks": [] }));
    h.send_json(json!({ "type": 7 }));
    h.send_json(json!({ "type": ["prompt"] }));
    h.send_json(json!({ "type": "wat", "blocks": [] }));
    h.send_json(json!({ "type": "session/prompt" }));
    h.expect_nothing_forwarded().await;
    h.expect_still_running().await;
}

#[tokio::test]
async fn a_set_mode_frame_is_discarded() {
    // Requirement 11 criterion 7. The command is gone from the wire, and
    // a client still sending it is tolerated in silence.
    let mut h = harness();
    h.send_json(json!({ "type": "set_mode", "modeId": "planner" }));
    h.send_json(json!({ "type": "set_mode" }));
    h.expect_nothing_forwarded().await;
    h.expect_still_running().await;
}

#[tokio::test]
async fn a_recognised_command_missing_a_required_field_is_discarded() {
    // Requirement 11 criteria 4 and 10. `prompt` requires `blocks` as a
    // JSON array, so the legacy `text` form goes; `permission_response`
    // requires an `id` that is a string or a number and an `optionId`
    // that is a string, so the empty-string substitution goes, which
    // would otherwise forward an answer naming no option; `set_model`
    // requires `modelId` as a string.
    let mut h = harness();
    // prompt
    h.send_json(json!({ "type": "prompt", "text": "the legacy form" }));
    h.send_json(json!({ "type": "prompt" }));
    h.send_json(json!({ "type": "prompt", "blocks": "not an array" }));
    h.send_json(json!({ "type": "prompt", "blocks": { "0": "text" } }));
    // permission_response
    h.send_json(json!({ "type": "permission_response", "optionId": "allow" }));
    h.send_json(json!({ "type": "permission_response", "id": "p1" }));
    h.send_json(json!({ "type": "permission_response", "id": "p1", "optionId": 3 }));
    h.send_json(json!({ "type": "permission_response", "id": null, "optionId": "allow" }));
    h.send_json(json!({ "type": "permission_response", "id": { "a": 1 }, "optionId": "allow" }));
    h.send_json(json!({ "type": "permission_response", "id": ["p1"], "optionId": "allow" }));
    // set_model
    h.send_json(json!({ "type": "set_model" }));
    h.send_json(json!({ "type": "set_model", "modelId": 42 }));
    h.send_json(json!({ "type": "set_model", "modelId": null }));
    h.expect_nothing_forwarded().await;
    h.expect_still_running().await;
}

#[tokio::test]
async fn the_four_surviving_commands_are_forwarded() {
    // The other half of the contract: what a well-formed frame does. An
    // unknown field on a recognised command is ignored, and block members
    // reach the Backend unchecked.
    let mut h = harness();
    h.send_json(json!({
        "type": "prompt",
        "blocks": [{ "type": "text", "text": "hi" }, { "type": "nonsense" }],
        "text": "ignored"
    }));
    h.send_json(json!({
        "type": "permission_response",
        "id": 12,
        "optionId": "allow",
        "remember": true
    }));
    h.send_json(json!({ "type": "set_model", "modelId": "sonnet" }));
    h.send_json(json!({ "type": "cancel", "sessionId": "ignored" }));

    let mut forwarded = Vec::new();
    for _ in 0..4 {
        let cmd = timeout(Duration::from_secs(2), h.commands.recv())
            .await
            .expect("forwarded within 2s")
            .expect("the channel is open");
        forwarded.push(cmd);
    }

    match &forwarded[0] {
        HubCommand::Prompt { blocks, attach_id } => {
            assert_eq!(*attach_id, ATTACH_ID, "the prompt carries this attach id");
            assert_eq!(blocks.len(), 2, "every block member is forwarded unchecked");
            assert_eq!(blocks[1], json!({ "type": "nonsense" }));
        }
        other => panic!("expected a Prompt, got {other:?}"),
    }
    match &forwarded[1] {
        HubCommand::PermissionResponse { id, option_id } => {
            assert_eq!(*id, json!(12), "a numeric id is accepted");
            assert_eq!(option_id, "allow");
        }
        other => panic!("expected a PermissionResponse, got {other:?}"),
    }
    match &forwarded[2] {
        HubCommand::SetModel { model_id } => assert_eq!(model_id, "sonnet"),
        other => panic!("expected a SetModel, got {other:?}"),
    }
    assert!(matches!(forwarded[3], HubCommand::Cancel));
}

#[tokio::test]
async fn the_loop_returns_when_the_inbound_stream_ends() {
    // Requirement 8 criterion 8, first clause. `channel_stream` is an
    // `unfold`, which panics if polled after its `None`; a `continue` on
    // the stream-ended arm therefore fails the join rather than spinning,
    // which is why the join result is asserted and the stream is not
    // fused. The other channels stay alive so no other arm can be the
    // one that ended the loop.
    let Harness {
        browser,
        commands: _commands,
        outbound: _outbound,
        sink: _sink,
        handle,
    } = harness();
    drop(browser);
    timeout(Duration::from_secs(2), handle)
        .await
        .expect("the loop returns within 2s once the stream ends")
        .expect("the loop returned without panicking");
}

#[tokio::test]
async fn the_loop_returns_when_the_hub_inbox_is_gone() {
    // Requirement 8 criterion 14. With the command receiver dropped, the
    // next forwarded command fails to send and the loop returns. The
    // browser sender stays alive for the whole wait: were it dropped the
    // stream would end and mask a `continue` on this arm.
    let Harness {
        browser,
        commands,
        outbound: _outbound,
        sink: _sink,
        handle,
    } = harness();
    drop(commands);
    browser
        .send(Message::Text(json!({ "type": "cancel" }).to_string()))
        .expect("the loop is still reading");
    timeout(Duration::from_secs(2), handle)
        .await
        .expect("the loop returns within 2s once the inbox is gone")
        .expect("the loop returned without panicking");
    drop(browser);
}

#[tokio::test]
async fn the_loop_returns_when_the_broadcast_channel_closes() {
    // Requirement 8 criterion 13. With every broadcast sender gone the
    // receiver reports `Closed` and the loop returns. A `continue` there
    // would spin; tokio's cooperative budget makes it yield, so this
    // case times out instead of hanging.
    let Harness {
        browser: _browser,
        commands: _commands,
        outbound,
        sink: _sink,
        handle,
    } = harness();
    drop(outbound);
    timeout(Duration::from_secs(2), handle)
        .await
        .expect("the loop returns within 2s once the broadcast closes")
        .expect("the loop returned without panicking");
}

#[tokio::test]
async fn a_lagged_receiver_skips_ahead_and_keeps_forwarding() {
    // Requirement 8 criterion 7. The loop task was only queued by
    // `harness()`: on this current-thread runtime it has not been polled
    // yet, so the twenty sends below overflow the 16-slot ring by four
    // before its first `recv`. The receiver then reports `Lagged(4)`, and
    // the loop must skip ahead and forward the sixteen that remain, in
    // order, with no frame describing the lag, and stay open for more.
    let Harness {
        browser: _browser,
        commands: _commands,
        outbound,
        mut sink,
        handle,
    } = harness();
    for i in 1..=20u32 {
        outbound
            .send(Arc::new(
                json!({ "type": "append", "role": "agent", "text": i.to_string() }),
            ))
            .expect("the loop holds a receiver");
    }

    let mut received: Vec<u32> = Vec::new();
    loop {
        let frame = timeout(Duration::from_secs(2), sink.recv())
            .await
            .expect("a frame within 2s")
            .expect("the loop is still writing");
        let Message::Text(text) = frame else {
            panic!("expected a text frame, got {frame:?}");
        };
        let value: Value = serde_json::from_str(&text).expect("the sink carries JSON");
        assert_eq!(value["type"], "append", "no frame describes the lag");
        let n: u32 = value["text"]
            .as_str()
            .expect("the text field")
            .parse()
            .expect("a published number");
        received.push(n);
        if n == 20 {
            break;
        }
    }
    assert_eq!(
        received,
        (5..=20).collect::<Vec<u32>>(),
        "the four evicted events are skipped and the rest arrive in order"
    );

    outbound
        .send(Arc::new(
            json!({ "type": "append", "role": "agent", "text": "21" }),
        ))
        .expect("the loop still holds a receiver");
    let frame = timeout(Duration::from_secs(2), sink.recv())
        .await
        .expect("the next frame within 2s")
        .expect("the loop is still writing");
    let Message::Text(text) = frame else {
        panic!("expected a text frame, got {frame:?}");
    };
    let value: Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(value["text"], "21", "the loop keeps forwarding after a lag");
    assert!(!handle.is_finished(), "a lag does not end the attach");
}
