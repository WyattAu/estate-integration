#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 5, suite 2 — `book_feed`: wire-kit 0.1.0 + book-kit 0.1.0.
//!
//! The wire→book pipeline the way a market-data host builds it: a FIX
//! 4.4 market-data feed is **encoded** with wire-kit's `FixBuilder`
//! (tag=value bodies, canonical `8=/9=/10=` framing), **decoded** with
//! `FixMessage::parse` (checksum verified, repeating group walked
//! zero-copy), **mapped** by the host into book-kit `Command`s
//! (`AddBid`/`AddAsk`/`Execute` — the typed side variants, never a
//! bool), and **replayed** into a `Book` through `book_kit::replay`
//! with an explicit `GapPolicy` and FIX tag 34 as the producer
//! sequence. The assertions cover the whole pipeline: framing validity,
//! checksum integrity (and its failure mode), replay report exactness,
//! depth state after partial and full fills, and the determinism
//! contract — the replayed book is checksum-identical to the same
//! commands applied directly.
//!
//! Host-side mapping (documented here because both crates are
//! deliberately unopinionated about it):
//!
//! | FIX tag | meaning | maps to |
//! |---|---|---|
//! | 34 | MsgSeqNum | `FeedEvent::seq` |
//! | 52 | SendingTime | `Command::ts_mono` (producer nanos, opaque) |
//! | 268 | NoMDEntries | repeating group count |
//! | 269 | MDEntryType: 0=Bid, 1=Offer, 2=Trade | command selection |
//! | 37 | OrderID | `OrderId` |
//! | 270 / 271 | MDEntryPx / MDEntrySize | `Price` (ticks) / `Qty` |

use book_kit::{Book, BookBuf, Command, GapPolicy, OrderId, ReplayError};
use wire_kit::fix::{FixBuilder, FixMessage, FixMsgType};

/// One market-data entry as the producer emits it.
#[derive(Clone, Copy, Debug)]
enum FeedEntry {
    /// 269=0 — a resting bid.
    Bid {
        /// OrderID (37).
        id: u64,
        /// MDEntryPx (270) in ticks.
        price: i64,
        /// MDEntrySize (271).
        qty: u64,
    },
    /// 269=1 — a resting offer.
    Ask {
        /// OrderID (37).
        id: u64,
        /// MDEntryPx (270) in ticks.
        price: i64,
        /// MDEntrySize (271).
        qty: u64,
    },
    /// 269=2 — a trade against a resting order.
    Trade {
        /// OrderID (37) of the resting order filled.
        id: u64,
        /// Traded quantity (271).
        qty: u64,
    },
}

/// Encodes one `35=X` (MarketDataIncrementalRefresh) frame with
/// wire-kit's `FixBuilder`: header tags, the 268 group count, then one
/// field run per entry. Returns the finished, checksummed frame.
fn encode_md_refresh(seq: u64, sending_time_ns: u64, entries: &[FeedEntry]) -> Vec<u8> {
    let mut buf = vec![0_u8; 1024];
    let mut b = FixBuilder::new(&mut buf).expect("buffer holds the frame");
    b.field_str(35, "X").expect("msg type"); // MsgType
    b.field_str(49, "FEED").expect("sender"); // SenderCompID
    b.field_str(56, "BOOK").expect("target"); // TargetCompID
    b.field_str(34, &seq.to_string()).expect("seq"); // MsgSeqNum
    b.field_str(52, &sending_time_ns.to_string())
        .expect("sending time"); // SendingTime (producer nanos)
    b.field_str(268, &entries.len().to_string())
        .expect("group count"); // NoMDEntries
    for entry in entries {
        match *entry {
            FeedEntry::Bid { id, price, qty } => {
                b.field_str(269, "0").expect("entry type bid");
                b.field_str(37, &id.to_string()).expect("order id");
                b.field_str(270, &price.to_string()).expect("price");
                b.field_str(271, &qty.to_string()).expect("qty");
            }
            FeedEntry::Ask { id, price, qty } => {
                b.field_str(269, "1").expect("entry type ask");
                b.field_str(37, &id.to_string()).expect("order id");
                b.field_str(270, &price.to_string()).expect("price");
                b.field_str(271, &qty.to_string()).expect("qty");
            }
            FeedEntry::Trade { id, qty } => {
                b.field_str(269, "2").expect("entry type trade");
                b.field_str(37, &id.to_string()).expect("order id");
                b.field_str(271, &qty.to_string()).expect("qty");
            }
        }
    }
    b.finish().expect("framing fits").to_vec()
}

/// Decodes one frame into its FIX message sequence number and the raw
/// commands its entries carry — the host's only glue between the two
/// crates. The FIX session numbers frames (`34=<n>`); the book journal
/// numbers *events*, so the pipeline assigns dense per-command journal
/// seqs across frames (see `map_frames`).
fn decode_md_refresh(frame: &[u8]) -> (u64, Vec<Command>) {
    let message = FixMessage::parse(frame).expect("well-formed frame parses");
    message.verify_checksum().expect("checksum holds");
    assert_eq!(
        message.message_type().expect("known type"),
        FixMsgType::MarketDataIncrementalRefresh,
        "35=X expected"
    );
    let seq: u64 = message
        .field_str(34)
        .expect("seq")
        .parse()
        .expect("numeric seq");
    let ts_mono: u64 = message
        .field_str(52)
        .expect("sending time")
        .parse()
        .expect("numeric sending time");
    let group = message.group(268).expect("NoMDEntries group");
    let mut commands = Vec::new();
    for entry in group.iter() {
        let entry = entry.expect("group entry decodes");
        let id = OrderId::new(
            entry
                .field_str(37)
                .expect("order id")
                .parse()
                .expect("numeric id"),
        );
        let command = match entry.field_str(269).expect("entry type") {
            "0" => Command::AddBid {
                id,
                price: entry
                    .field_str(270)
                    .expect("price")
                    .parse()
                    .expect("numeric price"),
                qty: entry
                    .field_str(271)
                    .expect("qty")
                    .parse()
                    .expect("numeric qty"),
                ts_mono,
            },
            "1" => Command::AddAsk {
                id,
                price: entry
                    .field_str(270)
                    .expect("price")
                    .parse()
                    .expect("numeric price"),
                qty: entry
                    .field_str(271)
                    .expect("qty")
                    .parse()
                    .expect("numeric qty"),
                ts_mono,
            },
            "2" => Command::Execute {
                id,
                qty: entry
                    .field_str(271)
                    .expect("qty")
                    .parse()
                    .expect("numeric qty"),
            },
            other => panic!("unknown MDEntryType {other}"),
        };
        commands.push(command);
    }
    (seq, commands)
}

/// Maps whole frames into the journalled feed: FIX-level sequence gaps
/// are detected at the frame boundary (a lost FIX message is a transport
/// gap), while every decoded command gets a dense journal seq. A command
/// whose journal seq falls in `skip` is consumed-but-not-fed — the
/// exactly shape of a lost journal record.
fn map_frames(frames: &[&[u8]], skip: Option<usize>) -> Vec<(u64, Command)> {
    let mut feed = Vec::new();
    let mut journal_seq = 0_u64;
    for (frame_index, frame) in frames.iter().enumerate() {
        let (msg_seq, commands) = decode_md_refresh(frame);
        assert_eq!(
            msg_seq,
            u64::try_from(frame_index).unwrap_or(u64::MAX) + 1,
            "a FIX-level sequence gap must be caught at the frame boundary"
        );
        for command in commands {
            let global_index = journal_seq as usize;
            if skip != Some(global_index) {
                feed.push((journal_seq, command));
            }
            journal_seq += 1;
        }
    }
    feed
}

/// The three-frame feed: adds on both sides, a level deepening, a
/// partial fill, and a full fill that empties a price level.
fn three_frame_feed() -> Vec<Vec<u8>> {
    vec![
        // seq 1: open the spread.
        encode_md_refresh(
            1,
            1_000,
            &[
                FeedEntry::Bid {
                    id: 7,
                    price: 99,
                    qty: 10,
                },
                FeedEntry::Ask {
                    id: 11,
                    price: 101,
                    qty: 8,
                },
            ],
        ),
        // seq 2: deepen the bid level, open a second ask level.
        encode_md_refresh(
            2,
            2_000,
            &[
                FeedEntry::Bid {
                    id: 8,
                    price: 99,
                    qty: 5,
                },
                FeedEntry::Ask {
                    id: 12,
                    price: 103,
                    qty: 6,
                },
            ],
        ),
        // seq 3: partial fill of bid 7, full fill of ask 11.
        encode_md_refresh(
            3,
            3_000,
            &[
                FeedEntry::Trade { id: 7, qty: 4 },
                FeedEntry::Trade { id: 11, qty: 8 },
            ],
        ),
    ]
}

/// The full pipeline: encode → decode → map → replay → assert depth,
/// checksum, and report exactness.
#[test]
fn fix_market_data_feed_replays_into_a_book_with_exact_depth() {
    let frames = three_frame_feed();
    assert_eq!(frames.len(), 3);

    // -- Wire half: every frame carries the canonical FIX 4.4 framing.
    for frame in &frames {
        let message = FixMessage::parse(frame).expect("frame parses");
        assert_eq!(
            message.field_str(8).expect("begin string"),
            "FIX.4.4",
            "finish() must prepend 8=FIX.4.4"
        );
        assert!(
            message.field(9).is_some(),
            "finish() must prepend the 9=<BodyLength> header"
        );
        assert!(
            message.field(10).is_some(),
            "finish() must append the 10=<checksum> trailer"
        );
    }

    // -- Map half: 6 entries across 3 frames become 6 journalled events.
    let refs: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
    let feed = map_frames(&refs, None);
    assert_eq!(feed.len(), 6, "3 frames × 2 entries each");
    assert_eq!(
        feed[0],
        (
            0,
            Command::AddBid {
                id: OrderId::new(7),
                price: 99,
                qty: 10,
                ts_mono: 1_000
            }
        ),
        "the first mapped event is the typed AddBid with the wire values"
    );

    // -- Book half: replay under a strict gap policy.
    let mut book = Book::with_capacity(64, 64);
    let report = book_kit::replay(&mut book, feed.iter().copied(), GapPolicy::Fail)
        .expect("dense journal seqs 0..=5 replay cleanly");
    assert_eq!(report.applied, 6);
    assert_eq!(report.gaps_skipped, 0);
    assert_eq!(report.first_seq, Some(0));
    assert_eq!(report.last_seq, Some(5));

    // -- Depth state after the fills: bid level 99 holds 10 − 4 + 5,
    //    the fully-filled ask level 101 is gone, 103 is the best offer.
    let reader = book.reader();
    let mut buf = BookBuf::new(16, 64);
    let version = reader.try_read(&mut buf).expect("quiet book reads");
    assert!(version > 0, "a mutated book publishes a nonzero version");

    let bids = reader.bids(&buf);
    assert_eq!(bids.level_count(), 1, "both bids rest on one price level");
    let best_bid = bids.best().expect("the bid level");
    assert_eq!(best_bid.price(), 99);
    assert_eq!(best_bid.qty(), 11, "10 − 4 (partial) + 5 (deepening)");
    assert_eq!(best_bid.order_count(), 2);

    let asks = reader.asks(&buf);
    assert_eq!(
        asks.level_count(),
        1,
        "the fully-filled 101 level must vanish"
    );
    let best_ask = asks.best().expect("the remaining ask level");
    assert_eq!(best_ask.price(), 103, "the next level becomes best");
    assert_eq!(best_ask.qty(), 6);

    // -- Determinism: the same commands applied directly (no replay
    //    machinery) must reach the checksum-identical state, and the
    //    partial fill's `Applied` report is exact on that path.
    let mut direct = Book::with_capacity(64, 64);
    for (seq, command) in feed {
        if matches!(command, Command::Execute { id, .. } if id == OrderId::new(7)) {
            let applied = direct.apply(command).expect("partial fill applies");
            assert_eq!(applied.executed, 4, "the trade filled exactly 4");
            assert_eq!(applied.remaining, 6, "6 units rest after the fill");
            assert_eq!(
                applied.id,
                OrderId::new(7),
                "the report names the filled order"
            );
        } else {
            direct.apply(command).expect("feed command applies");
        }
        let _ = seq; // the direct path applies in feed order, no seq bookkeeping
    }
    assert_eq!(
        book.checksum(),
        direct.checksum(),
        "replay and direct application must reach checksum-identical state"
    );
}

/// The pipeline's failure modes are typed: a gap aborts the replay, a
/// truncated frame fails to parse, and a corrupted body fails the
/// checksum — never silent acceptance.
#[test]
fn feed_corruption_is_typed_at_every_stage() {
    let frames = three_frame_feed();

    // -- Journal gap (one command lost): GapPolicy::Fail names both
    //       seqs. The lost command's seq was consumed by the journal
    //       numbering — the replay sees 0, 1, then 3.
    let refs: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
    let gapped = map_frames(&refs, Some(2));
    assert_eq!(gapped.len(), 5, "one command of six was lost");
    let mut book = Book::with_capacity(64, 64);
    let err = book_kit::replay(&mut book, gapped, GapPolicy::Fail)
        .expect_err("the missing journal seq 2 must abort the replay");
    assert_eq!(
        err,
        ReplayError::Gap {
            expected: 2,
            found: 3
        }
    );

    // -- Truncation: a frame cut mid-checksum is InsufficientBytes, not
    //    a panic and not a short parse.
    let truncated = &frames[0][..frames[0].len() - 3];
    assert!(matches!(
        FixMessage::parse(truncated),
        Err(wire_kit::WireError::InsufficientBytes { .. })
    ));

    // -- Body corruption: `parse` itself validates the tag-10 checksum
    //    (and rejects the frame), and the standalone `verify_checksum`
    //    re-check a host can run on a held message agrees.
    let mut tampered = frames[1].clone();
    let last_soh = tampered.len() - 1; // the frame ends `10=<digits>\x01`
    tampered[last_soh - 1] ^= 0x01; // flip one bit of the declared checksum
    assert!(matches!(
        FixMessage::parse(&tampered),
        Err(wire_kit::WireError::BadChecksum { .. })
    ));
    let clean = FixMessage::parse(&frames[1]).expect("the untouched frame parses");
    clean.verify_checksum().expect("clean frame re-verifies");
}
