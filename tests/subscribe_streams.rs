//! `subscribe` kinds against a local WebSocket server.
//!
//! The CLI always connects to the public endpoint, so the process-level tests
//! stop at the `subscribed` line (`--max-events 0`) and check validation and
//! resolution. What goes on the wire, and how each channel's frames come back
//! through the SDK, is checked in-process: [`stream_target`] runs the same code
//! as the command, against a socket pointed at a mock server that records the
//! subscribe frames it receives and answers with captured frame shapes.

mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use base64::Engine;
use hyperliquid_cli::output::JsonRenderOptions;
use hyperliquid_cli::stream::{SubscribeTarget, stream_target};
use hyperliquid_cli::stream_bounds::StreamBounds;
use hyperliquid_cli::ws::BookAggregationArgs;
use hypersdk::Address;
use hypersdk::hypercore::WebSocket;
use predicates::prelude::*;
use serde_json::{Value, json};
use support::IsolatedHome;
use support::exchange::{ExchangeFixture, Hip3Dex};

const USER: &str = "0xdfc24b077bc1425ad1dea75bcb6f8158e10df303";

fn user() -> Address {
    USER.parse().unwrap()
}

// ---------------------------------------------------------------------------
// A just-enough WebSocket server: one connection, text frames only.
// ---------------------------------------------------------------------------

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in message.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0u8; 20];
    for (chunk, word) in out.chunks_mut(4).zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn handshake(stream: &mut TcpStream) {
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
    }
    let request = String::from_utf8(request).unwrap();
    let key = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim().to_string())
        })
        .expect("client sent a Sec-WebSocket-Key");
    let accept = base64::engine::general_purpose::STANDARD.encode(sha1(
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    ));
    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )
    .unwrap();
}

/// The next text frame the client sent, or `None` once it closes.
fn read_text(stream: &mut TcpStream) -> Option<String> {
    loop {
        let mut head = [0u8; 2];
        stream.read_exact(&mut head).ok()?;
        let opcode = head[0] & 0x0f;
        let mut len = u64::from(head[1] & 0x7f);
        if len == 126 {
            let mut ext = [0u8; 2];
            stream.read_exact(&mut ext).ok()?;
            len = u64::from(u16::from_be_bytes(ext));
        } else if len == 127 {
            let mut ext = [0u8; 8];
            stream.read_exact(&mut ext).ok()?;
            len = u64::from_be_bytes(ext);
        }
        let mut mask = [0u8; 4];
        if head[1] & 0x80 != 0 {
            stream.read_exact(&mut mask).ok()?;
        }
        let mut payload = vec![0u8; len as usize];
        stream.read_exact(&mut payload).ok()?;
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
        match opcode {
            1 => return String::from_utf8(payload).ok(),
            8 => return None,
            _ => {}
        }
    }
}

fn send_text(stream: &mut TcpStream, text: &str) {
    let bytes = text.as_bytes();
    let mut frame = vec![0x81];
    match bytes.len() {
        0..=125 => frame.push(bytes.len() as u8),
        126..=65535 => {
            frame.push(126);
            frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        }
        _ => {
            frame.push(127);
            frame.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(bytes);
    stream.write_all(&frame).unwrap();
}

struct MockWs {
    url: reqwest::Url,
    subscribes: mpsc::Receiver<Value>,
}

/// Accept one client; once it has sent `expected` subscribe frames, send `frames`.
fn mock_ws(expected: usize, frames: &[Value]) -> MockWs {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let (tx, subscribes) = mpsc::channel();
    let frames: Vec<String> = frames.iter().map(Value::to_string).collect();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        handshake(&mut stream);
        let mut received = 0;
        while let Some(text) = read_text(&mut stream) {
            let message: Value = serde_json::from_str(&text).unwrap();
            if message["method"] == "subscribe" {
                tx.send(message).unwrap();
                received += 1;
                if received == expected {
                    for frame in &frames {
                        send_text(&mut stream, frame);
                    }
                }
            }
        }
    });
    MockWs { url, subscribes }
}

/// Run `target` against a mock server that answers with `frames`: the NDJSON
/// lines written and the subscribe frames the server received.
async fn drive(
    target: &SubscribeTarget,
    frames: &[Value],
    max_events: usize,
) -> (Vec<Value>, Vec<Value>) {
    let mock = mock_ws(target.subscriptions().len(), frames);
    let socket = WebSocket::new(mock.url.clone());
    let mut output = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(20),
        stream_target(
            target,
            socket,
            StreamBounds {
                max_items: Some(max_events),
                idle: Some(Duration::from_secs(10)),
            },
            &mut output,
            &JsonRenderOptions::default(),
            false,
        ),
    )
    .await
    .expect("the stream ended within 20s")
    .expect("the stream succeeded");
    let lines = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (lines, mock.subscribes.try_iter().collect())
}

fn subscribe_frame(subscription: Value) -> Value {
    json!({"method": "subscribe", "subscription": subscription})
}

// ---------------------------------------------------------------------------
// Captured frame shapes (mainnet, read-only), trimmed.
// ---------------------------------------------------------------------------

fn fill() -> Value {
    json!({"coin":"BTC","px":"74513.0","sz":"0.00043","side":"B","time":1794233663114u64,
        "startPosition":"0.024","dir":"Open Long","closedPnl":"0.0",
        "hash":"0x9bb24f76663e33439d2c04486bcfcd020172005c013152153f7afac925320d2e",
        "oid":599569989334u64,"crossed":false,"fee":"0.004806","tid":555633606799470u64,
        "feeToken":"USDC","twapId":null})
}

fn snapshot_frame(channel: &str, list_key: &str) -> Value {
    json!({"channel": channel, "data": {"isSnapshot": true, "user": USER, list_key: []}})
}

fn live_frame(channel: &str, list_key: &str) -> Value {
    json!({"channel": channel, "data": {"isSnapshot": false, "user": USER, list_key: []}})
}

struct Kind {
    name: &'static str,
    target: SubscribeTarget,
    /// The `subscription` object of each frame the server must receive.
    subscriptions: Vec<Value>,
    frame: Value,
    /// `channel` of the event line the SDK re-serializes the frame as.
    channel: &'static str,
}

fn kinds() -> Vec<Kind> {
    vec![
        Kind {
            name: "bbo",
            target: SubscribeTarget::bbo("BTC".into()),
            subscriptions: vec![json!({"type": "bbo", "coin": "BTC"})],
            frame: json!({"channel":"bbo","data":{"bbo":[{"n":17,"px":"74511.0","sz":"2.37869"},{"n":26,"px":"74512.0","sz":"6.35079"}],"coin":"BTC","time":1794233553742u64}}),
            channel: "bbo",
        },
        Kind {
            name: "active-asset-ctx perp",
            target: SubscribeTarget::active_asset_ctx("BTC".into()),
            subscriptions: vec![json!({"type": "activeAssetCtx", "coin": "BTC"})],
            frame: json!({"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{"dayBaseVlm":"42818.79","dayNtlVlm":"3204551598.55","funding":"0.0000125","impactPxs":["74511.0","74512.0"],"markPx":"74511.0","midPx":"74511.5","openInterest":"42204.57","oraclePx":"74527.0","premium":"-0.0002012693","prevDayPx":"75950.0"}}}),
            channel: "activeAssetCtx",
        },
        Kind {
            name: "active-asset-ctx spot",
            target: SubscribeTarget::active_asset_ctx("@142".into()),
            subscriptions: vec![json!({"type": "activeAssetCtx", "coin": "@142"})],
            frame: json!({"channel":"activeSpotAssetCtx","data":{"coin":"@142","ctx":{"prevDayPx":"75885.0","dayNtlVlm":"42451576.37","markPx":"74518.0","midPx":"74517.5","circulatingSupply":"20999999.99","coin":"@142","totalSupply":"20999999.99","dayBaseVlm":"567.58"}}}),
            channel: "activeSpotAssetCtx",
        },
        Kind {
            name: "active-asset-data",
            target: SubscribeTarget::active_asset_data(user(), "BTC".into()),
            subscriptions: vec![json!({"type": "activeAssetData", "user": USER, "coin": "BTC"})],
            frame: json!({"channel":"activeAssetData","data":{"availableToTrade":["7544183.9","7544183.9"],"coin":"BTC","leverage":{"rawUsd":null,"type":"cross","value":"20"},"markPx":"74511.0","maxTradeSzs":["2024.98","2024.98"],"user":USER}}),
            channel: "activeAssetData",
        },
        Kind {
            name: "user-events",
            target: SubscribeTarget::user_events(user()),
            subscriptions: vec![json!({"type": "userEvents", "user": USER})],
            // The exchange names this channel `user`, not `userEvents`.
            frame: json!({"channel":"user","data":{"fills":[fill()]}}),
            channel: "userEvents",
        },
        Kind {
            name: "user-fundings",
            target: SubscribeTarget::user_fundings(user()),
            subscriptions: vec![json!({"type": "userFundings", "user": USER})],
            frame: snapshot_frame("userFundings", "fundings"),
            channel: "userFundings",
        },
        Kind {
            name: "notifications",
            target: SubscribeTarget::notifications(user()),
            subscriptions: vec![json!({"type": "notification", "user": USER})],
            frame: json!({"channel":"notification","data":{"notification":"Order filled"}}),
            channel: "notification",
        },
        Kind {
            name: "twap-states",
            target: SubscribeTarget::twap_states(user(), vec![None]),
            subscriptions: vec![json!({"type": "twapStates", "user": USER})],
            frame: json!({"channel":"twapStates","data":{"dex":"","states":[],"user":USER}}),
            channel: "twapStates",
        },
        Kind {
            name: "twap-slice-fills",
            target: SubscribeTarget::twap_slice_fills(user()),
            subscriptions: vec![json!({"type": "userTwapSliceFills", "user": USER})],
            frame: snapshot_frame("userTwapSliceFills", "twapSliceFills"),
            channel: "userTwapSliceFills",
        },
        Kind {
            name: "clearinghouse-state dex",
            target: SubscribeTarget::clearinghouse_state(user(), vec![Some("xyz".into())]),
            subscriptions: vec![json!({"type": "clearinghouseState", "user": USER, "dex": "xyz"})],
            frame: json!({"channel":"clearinghouseState","data":{"clearinghouseState":{"assetPositions":[],"crossMaintenanceMarginUsed":"0.0","crossMarginSummary":{"accountValue":"7544191.25","totalMarginUsed":"0.0","totalNtlPos":"0.0","totalRawUsd":"7544191.25"},"marginSummary":{"accountValue":"7544669.12","totalMarginUsed":"477.86","totalNtlPos":"0.0","totalRawUsd":"7544669.12"},"time":1794233604773u64,"withdrawable":"7544191.25"},"dex":"xyz","user":USER}}),
            channel: "clearinghouseState",
        },
        Kind {
            name: "open-orders",
            target: SubscribeTarget::open_orders(user(), vec![None]),
            subscriptions: vec![json!({"type": "openOrders", "user": USER})],
            frame: json!({"channel":"openOrders","data":{"dex":"","orders":[],"user":USER}}),
            channel: "openOrders",
        },
        Kind {
            name: "spot-state",
            target: SubscribeTarget::spot_state(user()),
            subscriptions: vec![json!({"type": "spotState", "user": USER})],
            frame: json!({"channel":"spotState","data":{"spotState":{"balances":[]},"user":USER}}),
            channel: "spotState",
        },
        Kind {
            name: "all-mids dex",
            target: SubscribeTarget::all_mids(Some("xyz".into())),
            subscriptions: vec![json!({"type": "allMids", "dex": "xyz"})],
            frame: json!({"channel":"allMids","data":{"dex":"xyz","mids":{"xyz:XYZ100":"123.5"}}}),
            channel: "allMids",
        },
        Kind {
            name: "all-mids default dex sends no dex",
            target: SubscribeTarget::all_mids(None),
            subscriptions: vec![json!({"type": "allMids"})],
            frame: json!({"channel":"allMids","data":{"mids":{"BTC":"50000"}}}),
            channel: "allMids",
        },
        Kind {
            name: "orderbook aggregated",
            target: SubscribeTarget::orderbook(
                "BTC".into(),
                BookAggregationArgs {
                    n_sig_figs: Some(5),
                    mantissa: Some(2),
                },
            ),
            subscriptions: vec![
                json!({"type": "l2Book", "coin": "BTC", "nSigFigs": 5, "mantissa": 2}),
            ],
            frame: json!({"channel":"l2Book","data":{"coin":"BTC","time":1794233553742u64,"levels":[[{"n":14,"px":"74512.0","sz":"4.43964"}],[{"n":9,"px":"74514.0","sz":"1.5"}]]}}),
            channel: "l2Book",
        },
        Kind {
            name: "orderbook full precision",
            target: SubscribeTarget::orderbook("BTC".into(), BookAggregationArgs::default()),
            subscriptions: vec![json!({"type": "l2Book", "coin": "BTC"})],
            frame: json!({"channel":"l2Book","data":{"coin":"BTC","time":1794233553742u64,"levels":[[{"n":14,"px":"74511.0","sz":"4.43964"}],[{"n":9,"px":"74512.0","sz":"1.5"}]]}}),
            channel: "l2Book",
        },
        Kind {
            name: "fills",
            target: SubscribeTarget::fills(user()),
            subscriptions: vec![json!({"type": "userFills", "user": USER})],
            frame: snapshot_frame("userFills", "fills"),
            channel: "userFills",
        },
        Kind {
            name: "order-updates",
            target: SubscribeTarget::order_updates(user()),
            subscriptions: vec![json!({"type": "orderUpdates", "user": USER})],
            frame: json!({"channel":"orderUpdates","data":[{"order":{"coin":"BTC","side":"B","limitPx":"74510.0","sz":"0.37","oid":599570053978u64,"timestamp":1794233664760u64,"origSz":"0.37","cloid":"0x0000000000000000000001a1bd313f9a"},"status":"open","statusTimestamp":1794233664760u64}]}),
            channel: "orderUpdates",
        },
    ]
}

#[tokio::test]
async fn every_subscribe_kind_sends_its_documented_frame_and_streams_its_channel() {
    for kind in kinds() {
        let (lines, subscribes) = drive(&kind.target, std::slice::from_ref(&kind.frame), 1).await;

        let expected: Vec<Value> = kind
            .subscriptions
            .iter()
            .cloned()
            .map(subscribe_frame)
            .collect();
        assert_eq!(subscribes, expected, "{}: subscribe frames", kind.name);
        assert_eq!(lines.len(), 2, "{}: {lines:?}", kind.name);
        assert_eq!(lines[0]["type"], "subscribed", "{}", kind.name);
        assert_eq!(lines[1]["type"], "event", "{}", kind.name);
        assert_eq!(lines[1]["data"]["channel"], kind.channel, "{}", kind.name);
    }
}

#[tokio::test]
async fn repeated_dex_flags_subscribe_once_per_distinct_dex_on_one_connection() {
    let target = SubscribeTarget::clearinghouse_state(
        user(),
        vec![None, Some("xyz".into()), None, Some("xyz".into())],
    );
    let frame = json!({"channel":"openOrders","data":{"dex":"","orders":[],"user":USER}});
    // The matcher is for clearinghouse state, so the frame is ignored; only the
    // subscribe frames and the label are under test, and the stream ends on idle.
    let mock = mock_ws(2, &[frame]);
    let socket = WebSocket::new(mock.url.clone());
    let mut output = Vec::new();
    let error = stream_target(
        &target,
        socket,
        StreamBounds {
            max_items: Some(1),
            idle: Some(Duration::from_millis(600)),
        },
        &mut output,
        &JsonRenderOptions::default(),
        false,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("Timed out"), "{error}");

    let received: Vec<Value> = mock.subscribes.try_iter().collect();
    assert_eq!(
        received,
        vec![
            subscribe_frame(json!({"type": "clearinghouseState", "user": USER})),
            subscribe_frame(json!({"type": "clearinghouseState", "user": USER, "dex": "xyz"})),
        ]
    );
    let first: Value =
        serde_json::from_str(String::from_utf8(output).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!(
        first["subscription"],
        format!(
            "clearinghouseState({user},None), clearinghouseState({user},Some(\"xyz\"))",
            user = user()
        )
    );
}

#[tokio::test]
async fn skip_snapshot_drops_the_snapshot_and_counts_only_live_events() {
    type Build = fn() -> SubscribeTarget;
    let cases: [(&str, Build, &str, &str); 3] = [
        (
            "fills",
            || SubscribeTarget::fills(user()),
            "userFills",
            "fills",
        ),
        (
            "user-fundings",
            || SubscribeTarget::user_fundings(user()),
            "userFundings",
            "fundings",
        ),
        (
            "twap-slice-fills",
            || SubscribeTarget::twap_slice_fills(user()),
            "userTwapSliceFills",
            "twapSliceFills",
        ),
    ];
    for (name, build, channel, list_key) in cases {
        let frames = [
            snapshot_frame(channel, list_key),
            live_frame(channel, list_key),
        ];

        // Default: the snapshot is the first event, exactly as before.
        let (lines, _) = drive(&build(), &frames, 1).await;
        assert_eq!(lines.len(), 2, "{name}: {lines:?}");
        assert_eq!(lines[1]["data"]["data"]["isSnapshot"], true, "{name}");

        // Skipped: the first (and only counted) event is the live one.
        let (lines, _) = drive(&build().skipping_snapshot(true), &frames, 1).await;
        assert_eq!(lines.len(), 2, "{name}: {lines:?}");
        assert_eq!(lines[1]["data"]["data"]["isSnapshot"], false, "{name}");
    }
}

#[tokio::test]
async fn skip_snapshot_keeps_a_snapshot_flagged_event_countable_when_not_skipped() {
    // Two snapshot-flagged frames, both counted when nothing is skipped.
    let frames = [
        snapshot_frame("userFills", "fills"),
        snapshot_frame("userFills", "fills"),
    ];
    let (lines, _) = drive(&SubscribeTarget::fills(user()), &frames, 2).await;
    assert_eq!(lines.len(), 3, "{lines:?}");
}

#[tokio::test]
async fn skip_snapshot_does_not_affect_live_fills_delivered_as_user_events() {
    // `fills` also accepts fills the exchange sends on the `user` channel, which
    // has no snapshot flag; skipping snapshots must keep them.
    let frames = [json!({"channel":"user","data":{"fills":[fill()]}})];
    let (lines, _) = drive(
        &SubscribeTarget::fills(user()).skipping_snapshot(true),
        &frames,
        1,
    )
    .await;
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[1]["data"]["channel"], "userEvents");
}

// ---------------------------------------------------------------------------
// Process level: validation, resolution and the `subscribed` line.
// ---------------------------------------------------------------------------

fn subscribed_line(assert: &assert_cmd::assert::Assert) -> Value {
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    serde_json::from_str(stdout.lines().next().expect("a subscribed line")).unwrap()
}

#[test]
fn user_streams_read_the_given_user_without_a_signer() {
    let home = IsolatedHome::new();
    for kind in [
        "fills",
        "order-updates",
        "user-events",
        "user-fundings",
        "notifications",
        "twap-states",
        "twap-slice-fills",
        "clearinghouse-state",
        "open-orders",
        "spot-state",
    ] {
        let assert = home
            .command()
            .args(["subscribe", kind, "--user", USER, "--max-events", "0"])
            .assert()
            .success();
        let subscription = subscribed_line(&assert)["subscription"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(
            subscription.contains(USER),
            "{kind} subscribes for the --user address: {subscription}"
        );
    }
}

#[test]
fn user_streams_without_user_or_signer_need_authentication() {
    let home = IsolatedHome::new();
    home.command()
        .args(["subscribe", "fills", "--max-events", "0"])
        .assert()
        .failure()
        .code(10)
        .stderr(predicate::str::contains("Authentication required"));
}

#[test]
fn user_streams_default_to_the_selected_signer() {
    let home = IsolatedHome::new();
    let mut command = home.command();
    command.env(support::PRIVATE_KEY_ENV, support::VALID_PRIVATE_KEY);
    let assert = command
        .args(["subscribe", "fills", "--max-events", "0"])
        .assert()
        .success();
    let subscription = subscribed_line(&assert)["subscription"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(
        subscription
            .contains(&support::expected_address(support::VALID_PRIVATE_KEY).to_ascii_lowercase()),
        "{subscription}"
    );
}

#[test]
fn skip_snapshot_is_offered_only_where_the_exchange_flags_a_snapshot() {
    let home = IsolatedHome::new();
    for (kind, offered) in [
        ("fills", true),
        ("user-fundings", true),
        ("twap-slice-fills", true),
        ("order-updates", false),
        ("user-events", false),
        ("clearinghouse-state", false),
    ] {
        let output = home
            .command()
            .args(["subscribe", kind, "--help"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let help = String::from_utf8(output).unwrap();
        assert_eq!(help.contains("--skip-snapshot"), offered, "{kind}");
    }
}

#[test]
fn orderbook_aggregation_is_validated() {
    let home = IsolatedHome::new();
    for args in [
        &["--n-sig-figs", "1"][..],
        &["--n-sig-figs", "6"][..],
        &["--n-sig-figs", "five"][..],
        &["--n-sig-figs", "5", "--mantissa", "3"][..],
        &["--n-sig-figs", "5", "--mantissa", "0"][..],
    ] {
        home.command()
            .args([
                "subscribe",
                "orderbook",
                "--asset",
                "BTC",
                "--max-events",
                "0",
            ])
            .args(args)
            .assert()
            .failure()
            .code(2);
    }
    // A mantissa needs nSigFigs 5: rejected before any market is looked up.
    for args in [
        &["--mantissa", "2"][..],
        &["--n-sig-figs", "4", "--mantissa", "2"][..],
    ] {
        home.command()
            .args([
                "subscribe",
                "orderbook",
                "--asset",
                "BTC",
                "--max-events",
                "0",
            ])
            .args(args)
            .assert()
            .failure()
            .code(2)
            .stderr(predicate::str::contains(
                "--mantissa is only valid together with --n-sig-figs 5",
            ));
        home.command()
            .args(["book", "BTC"])
            .args(args)
            .assert()
            .failure()
            .code(2)
            .stderr(predicate::str::contains(
                "--mantissa is only valid together with --n-sig-figs 5",
            ));
    }
}

#[tokio::test]
async fn orderbook_label_names_the_aggregation() {
    let exchange = ExchangeFixture::start().await;
    let home = IsolatedHome::new();
    let assert = home
        .command_with_server(&exchange)
        .args([
            "subscribe",
            "orderbook",
            "--asset",
            "BTC",
            "--n-sig-figs",
            "5",
            "--mantissa",
            "2",
            "--max-events",
            "0",
        ])
        .assert()
        .success();
    assert_eq!(
        subscribed_line(&assert)["subscription"],
        "l2Book(BTC,nSigFigs=5,mantissa=2)"
    );

    // Without the flags the label is the SDK's, as before.
    let assert = home
        .command_with_server(&exchange)
        .args([
            "subscribe",
            "orderbook",
            "--asset",
            "BTC",
            "--max-events",
            "0",
        ])
        .assert()
        .success();
    assert_eq!(subscribed_line(&assert)["subscription"], "l2Book(BTC)");
}

#[tokio::test]
async fn dex_and_hip3_names_are_resolved_before_subscribing() {
    let exchange = ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await;
    let home = IsolatedHome::new();

    let run = |args: &[&str]| {
        home.command_with_server(&exchange)
            .args(["subscribe"])
            .args(args)
            .args(["--max-events", "0"])
            .assert()
    };

    // The canonical dex spelling goes on the wire.
    let assert = run(&["all-mids", "--dex", "XYZ"]).success();
    assert_eq!(
        subscribed_line(&assert)["subscription"],
        "allMids(Some(\"xyz\"))"
    );
    let assert = run(&["all-mids"]).success();
    assert_eq!(subscribed_line(&assert)["subscription"], "allMids(None)");

    // An unknown dex is refused instead of subscribing to nothing.
    run(&["all-mids", "--dex", "nope"]).failure().code(13);
    run(&["open-orders", "--user", USER, "--dex", "nope"])
        .failure()
        .code(13);

    // HIP-3 markets are subscribed by their dex-qualified name.
    let assert = run(&["bbo", "--asset", "xyz:XYZ100"]).success();
    assert_eq!(subscribed_line(&assert)["subscription"], "bbo(xyz:XYZ100)");
    // The name is the exchange's canonical one, not the input's spelling.
    let assert = run(&["bbo", "--asset", "btc"]).success();
    assert_eq!(subscribed_line(&assert)["subscription"], "bbo(BTC)");
    let assert = run(&["active-asset-ctx", "--asset", "xyz:XYZ100"]).success();
    assert_eq!(
        subscribed_line(&assert)["subscription"],
        "activeAssetCtx(xyz:XYZ100)"
    );
    let assert = run(&["active-asset-data", "--user", USER, "--asset", "xyz:XYZ100"]).success();
    assert!(
        subscribed_line(&assert)["subscription"]
            .as_str()
            .unwrap()
            .ends_with(",xyz:XYZ100)")
    );

    // Several dexes: one subscription each, default dex first as given.
    let assert = run(&[
        "clearinghouse-state",
        "--user",
        USER,
        "--dex",
        "xyz",
        "--dex",
        "xyz",
    ])
    .success();
    let label = subscribed_line(&assert)["subscription"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(label.matches("clearinghouseState(").count(), 1, "{label}");
    assert!(label.ends_with(",Some(\"xyz\"))"), "{label}");
}

#[tokio::test]
async fn repeated_dex_flags_are_validated_against_one_dex_listing() {
    let exchange = ExchangeFixture::start_with(vec![Hip3Dex::xyz()]).await;
    let home = IsolatedHome::new();

    home.command_with_server(&exchange)
        .args(["subscribe", "clearinghouse-state", "--user", USER])
        .args(["--dex", "xyz", "--dex", "XYZ", "--dex", "xyz"])
        .args(["--max-events", "0"])
        .assert()
        .success();

    assert_eq!(exchange.info_requests("perpDexs").await.len(), 1);
}

#[tokio::test]
async fn active_asset_data_is_for_perpetuals_only() {
    let exchange = ExchangeFixture::start().await;
    exchange
        .override_info(
            json!({"type": "spotMeta"}),
            json!({
                "universe": [{"name": "PURR/USDC", "index": 0, "tokens": [1, 0]}],
                "tokens": [
                    {"name": "USDC", "index": 0, "tokenId": "0x00000000000000000000000000000000", "szDecimals": 6, "weiDecimals": 6, "evmContract": null},
                    {"name": "PURR", "index": 1, "tokenId": "0x00000000000000000000000000000001", "szDecimals": 0, "weiDecimals": 0, "evmContract": null}
                ]
            }),
        )
        .await;
    let home = IsolatedHome::new();
    let run = |asset: &str| {
        home.command_with_server(&exchange)
            .args(["subscribe", "active-asset-data", "--user", USER, "--asset"])
            .arg(asset)
            .args(["--max-events", "0"])
            .assert()
    };

    // The market stream takes the spot pair; the per-user perp feed refuses it
    // locally instead of subscribing to something the exchange will not serve.
    home.command_with_server(&exchange)
        .args([
            "subscribe",
            "active-asset-ctx",
            "--asset",
            "PURR/USDC",
            "--max-events",
            "0",
        ])
        .assert()
        .success();
    run("PURR/USDC").failure().code(13);
    let assert = run("btc").success();
    assert!(
        subscribed_line(&assert)["subscription"]
            .as_str()
            .unwrap()
            .ends_with(",BTC)")
    );
}

#[tokio::test]
async fn a_command_cannot_subscribe_past_the_documented_subscription_limit() {
    let exchange = ExchangeFixture::start().await;
    let home = IsolatedHome::new();
    let dexes = |count: usize| {
        let mut command = home.command_with_server(&exchange);
        command.args([
            "subscribe",
            "clearinghouse-state",
            "--user",
            USER,
            "--max-events",
            "0",
        ]);
        for index in 0..count {
            command.args(["--dex", &format!("dex{index}")]);
        }
        command
    };

    dexes(1001)
        .assert()
        .failure()
        .code(13)
        .stderr(predicate::str::contains("1001 subscriptions requested"))
        .stderr(predicate::str::contains("at most 1000"));

    // At the limit the guard passes and the first unknown dex is what fails.
    dexes(1000)
        .assert()
        .failure()
        .code(13)
        .stderr(predicate::str::contains("subscriptions requested").not())
        .stderr(predicate::str::contains("Unknown DEX"));
}
