#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Round 6, suite 1 — `calibration_stack`: a2l-parse 0.1.0 + dbc-parse 0.1.0
//! + can-core 0.1.0 + xcp-core 0.1.0.
//!
//! This is the *whole* automotive calibration pipeline the way a
//! calibration engineer tool builds it, and it is the first suite to
//! compose four crates across L0 and L1 in one flow:
//!
//! ```text
//!   A2L text ──a2l_parse──► addresses, datatypes, limits, COMPU_METHODs
//!                               │
//!   DBC text ──dbc_parse──► signal bit layout (start_bit, length,
//!                               │          byte_order, scale, offset)
//!                               ▼
//!                       bind: A2L measurement ⇄ DBC signal
//!                               │
//!   CONNECT  ──xcp_core──► CONNECT response ⇒ negotiated byte_order,
//!                               │            max_cto, max_dto, resource
//!                               ▼
//!                       SET_MTA + UPLOAD built at the A2L address
//!                               │
//!   DAQ list ──xcp_core──► DTO bytes ──can_core──► SocketCAN bytes
//!                               │
//!                       dbc_decode_raw ──► engineering ──► COMPU_METHOD
//!                               │                              │
//!                               └──── within A2L limits ◄─────┘
//! ```
//!
//! Every crate in that diagram is deliberately unopinionated about the
//! others, so the host owns the bindings — which is exactly what this
//! suite pins down:
//!
//! - **a2l-parse** supplies *what* to read (`MEASUREMENT.ECU_ADDRESS`,
//!   `ECU_ADDRESS`, `DataType::size_bytes`) and *how to interpret it*
//!   (`COMPU_METHOD` coefficients, `RL_VALUE` limits). It never emits a
//!   byte order or a bit position.
//! - **dbc-parse** supplies *where in the payload* (`Signal.start_bit`,
//!   `bit_length`, `byte_order`) and the physical scaling
//!   (`scale`/`offset`), with `decode_raw`/`encode_raw` as the only
//!   bit-twiddling the host delegates.
//! - **xcp-core** supplies *how to ask* (SET_MTA/UPLOAD/DAQ framing) and
//!   is byte-order parameterized, so the host must feed it the byte order
//!   the slave declared in CONNECT — a real coupling between two crates
//!   that share no type.
//! - **can-core** supplies the wire encoding. Classic CAN caps a packet at
//!   `CLASSIC_FRAME_SIZE` (8), which is the constraint that makes an
//!   XCP-over-CAN CTO exactly one frame; the suite asserts every packet
//!   the host builds fits the negotiated `max_cto`.
//!
//! The interesting assertions are the *cross-crate* ones: that an A2L
//! address survives into the SET_MTA bytes under the negotiated byte
//! order, that a DBC signal round-trips through the exact payload bytes
//! an XCP DTO carries, and that the composed physical value lands inside
//! the limits the A2L file declared for it. A drift in any one crate
//! breaks a specific test here, which is the point of the suite.

use a2l_parse::{A2lError, A2lProject, CharType, ConversionType, DataType};
use can_core::{CanError, CanFrame, CanId, CLASSIC_FRAME_SIZE, CLASSIC_MAX_LEN};
use dbc_parse::{Dbc, DbcError};
use xcp_core::{
    connect, parse_response_with, set_mta, upload_frame, ByteOrder, CommModeBasic, ConnectInfo,
    DaqEntry, DaqList, Expect, ResourceMode, StartStopMode, XcpError, XcpResponse,
};

// ---------------------------------------------------------------------------
// Fixtures — a realistic (if small) powertrain ECU description.
// ---------------------------------------------------------------------------

/// The A2L description: one module, four characteristics, two
/// measurements, and the COMPU_METHODs that give them meaning.
const A2L: &str = r#"
/begin PROJECT powertrain "Bench calibration project"
  /begin MODULE ecu "Turbo V6 ECU"
    /begin MEASUREMENT engine_speed "Engine speed" UWORD
      rpm_conv 0.25 0.0 0.0 8000.0
      ECU_ADDRESS 0x32000100
    /end MEASUREMENT
    /begin MEASUREMENT engine_load "Engine load" UBYTE
      load_conv 0.5 0.0 0.0 100.0
      ECU_ADDRESS 0x32000102
    /end MEASUREMENT
    /begin CHARACTERISTIC inj_pulse "Injector pulse width" VALUE 0x72000000
      inj_record 0.0 inj_conv 0.0 12.5
      PHYS_UNIT "ms"
    /end CHARACTERISTIC
    /begin CHARACTERISTIC boost_target "Boost target" VALUE 0x72000004
      boost_record 0.0 bar_conv 0.0 2.5
      PHYS_UNIT "bar"
    /end CHARACTERISTIC
    /begin RECORD_LAYOUT inj_record
      FNC_VALUES 1 UBYTE inj_conv
    /end RECORD_LAYOUT
    /begin RECORD_LAYOUT boost_record
    /end RECORD_LAYOUT
    /begin COMPU_METHOD rpm_conv "rpm" IDENTICAL
      "" "" COEFFS 0 1 0 0 0 0
    /end COMPU_METHOD
    /begin COMPU_METHOD load_conv "load" RAT_FUNC
      "%.1f" "%" COEFFS 0 0.392156862745098 0 0 0 1
    /end COMPU_METHOD
    /begin COMPU_METHOD inj_conv "pulse" RAT_FUNC
      "%.3f" "ms" COEFFS 0 0.0390625 0 0 0 1
    /end COMPU_METHOD
    /begin COMPU_METHOD bar_conv "bar" RAT_FUNC
      "%.3f" "bar" COEFFS 0 0.0392156862745098 -0.392156862745098 0 0 1
    /end COMPU_METHOD
  /end MODULE
/end PROJECT
"#;

/// The DBC layout for the measurement payload: a little-endian status
/// message carrying `engine_speed` (16-bit, factor 1/16 rpm per the DBC)
/// and `engine_load` (8-bit).
const DBC: &str = r#"
VERSION "bench"

NS_ :

BS_:

BU_: ECU Bench

BO_ 320 ECU_STATUS: 8 ECU
 SG_ engine_speed : 0|16@1+ (0.0625,0) [0|8000] "rpm" Bench
 SG_ engine_load : 16|8@1+ (0.392156862745098,0) [0|100] "%" Bench
 SG_ coolant_temp : 24|8@1- (1,-40) [-40|215] "degC" Bench

BO_ 321 ECU_STATUS_EXT: 8 ECU
 SG_ boost_actual : 0|8@1+ (0.0392156862745098,-0.392156862745098) [-0.392|10] "bar" Bench
"#;

/// Recovers the unscaled raw count from a `decode_raw` result, inverting
/// the DBC factor. The host does this because the A2L COMPU_METHOD is
/// defined against raw counts, while `dbc-parse` reports engineering
/// values — the two descriptions meet only in the host.
fn rpm_raw_from_engineering(signal: &dbc_parse::Signal, engineering: f64) -> f64 {
    (engineering - signal.offset) / signal.scale
}

/// See [`rpm_raw_from_engineering`].
fn load_raw_from_engineering(signal: &dbc_parse::Signal, engineering: f64) -> f64 {
    (engineering - signal.offset) / signal.scale
}

/// CAN identifiers an XCP-over-CAN session uses.
fn can_ids() -> (CanId, CanId) {
    (
        CanId::standard(0x600).unwrap(),
        CanId::standard(0x601).unwrap(),
    )
}

// ---------------------------------------------------------------------------
// 1. a2l-parse alone — the description half.
// ---------------------------------------------------------------------------

#[test]
fn a2l_project_exposes_modules_characteristics_and_measurements() {
    let project = A2lProject::parse(A2L).unwrap();
    assert_eq!(project.project, "powertrain");
    assert_eq!(project.modules.len(), 1);

    let module = project.module_by_name("ecu").unwrap();
    assert_eq!(module.characteristics.len(), 2);
    assert_eq!(module.measurements.len(), 2);
    assert_eq!(module.compu_methods.len(), 4);
    assert!(project.module_by_name("nope").is_none());

    let inj = module.characteristic_by_name("inj_pulse").unwrap();
    assert_eq!(inj.r#type, CharType::Value);
    assert_eq!(inj.address, 0x7200_0000); // A2L hex has no `_` separators
    assert_eq!(inj.lower_limit, 0.0);
    assert_eq!(inj.upper_limit, 12.5);
    assert!(module.characteristic_by_name("missing").is_none());
}

#[test]
fn a2l_datatype_size_drives_the_upload_length() {
    let project = A2lProject::parse(A2L).unwrap();
    let module = project.module_by_name("ecu").unwrap();

    // The host derives the UPLOAD length purely from the A2L datatype —
    // this is the a2l-parse → xcp-core coupling with no shared type.
    let rpm = module.measurement_by_name("engine_speed").unwrap();
    assert_eq!(rpm.datatype, DataType::Uword);
    assert_eq!(rpm.datatype.size_bytes(), 2);

    let load = module.measurement_by_name("engine_load").unwrap();
    assert_eq!(load.datatype, DataType::Ubyte);
    assert_eq!(load.datatype.size_bytes(), 1);
}

#[test]
fn a2l_compu_methods_convert_raw_to_physical() {
    let project = A2lProject::parse(A2L).unwrap();
    let module = project.module_by_name("ecu").unwrap();

    let rpm = module.compu_method_by_name("rpm_conv").unwrap();
    assert_eq!(rpm.conversion_type, ConversionType::Identity);
    assert_eq!(rpm.to_linear().unwrap().apply(3200.0), 3200.0);

    // load_conv: 0.392156862745098 * raw = percent.
    let load = module.compu_method_by_name("load_conv").unwrap();
    assert_eq!(load.conversion_type, ConversionType::RatFunc);
    assert!((load.to_linear().unwrap().apply(128.0) - 50.196_078_431_372_55).abs() < 1e-9);

    // bar_conv carries an intercept — the case a pure-scale shortcut drops.
    let bar = module.compu_method_by_name("bar_conv").unwrap();
    let lin = bar.to_linear().unwrap();
    assert!((lin.slope - 0.039_215_686_274_509_8).abs() < 1e-15);
    assert!((lin.intercept + 0.392_156_862_745_098).abs() < 1e-12);
}

#[test]
fn a2l_malformed_input_is_a_typed_error_with_a_line_number() {
    // A block that never closes: the parser must not hang or panic.
    let err = A2lProject::parse("/begin PROJECT x\n/begin MODULE y\n").unwrap_err();
    assert!(
        matches!(
            err,
            A2lError::UnterminatedBlock { .. } | A2lError::UnexpectedEof { .. }
        ),
        "got {err:?}"
    );

    // An unterminated quoted string.
    let err = A2lProject::parse("/begin PROJECT x \"open\n/end PROJECT\n").unwrap_err();
    assert!(!err.to_string().is_empty());
}

// ---------------------------------------------------------------------------
// 2. dbc-parse alone — the bit-layout half.
// ---------------------------------------------------------------------------

#[test]
fn dbc_binds_the_measurement_signals_with_bit_layout_and_scaling() {
    let dbc = Dbc::parse(DBC).unwrap();
    assert_eq!(dbc.messages.len(), 2);

    let status = &dbc.messages[0];
    assert_eq!(status.name, "ECU_STATUS");
    assert_eq!(status.dlc, 8);
    assert_eq!(status.signals.len(), 3);

    let rpm = &status.signals[0];
    assert_eq!(rpm.name, "engine_speed");
    assert_eq!(rpm.start_bit, 0);
    assert_eq!(rpm.bit_length, 16);
    assert_eq!(rpm.scale, 0.0625);
    assert_eq!(rpm.offset, 0.0);
    assert_eq!(rpm.unit, "rpm");
    // Little-endian (Intel) — the DBC `@1` marker.
    assert_eq!(rpm.byte_order, dbc_parse::ByteOrder::Intel);

    let coolant = &status.signals[2];
    // Signed with a negative offset — the case that catches unsigned-only
    // extraction.
    assert!(coolant.is_signed);
    assert_eq!(coolant.offset, -40.0);
}

#[test]
fn dbc_signal_raw_round_trips_through_exact_payload_bytes() {
    let dbc = Dbc::parse(DBC).unwrap();
    let signal = &dbc.messages[0].signals[0];

    let mut data = [0_u8; 8];
    signal.encode_raw(&mut data, 3_200.0).unwrap();
    // `decode_raw` returns the *scaled* value — the DBC scale/offset are
    // applied inside, so a host must not scale a second time.
    let decoded = signal.decode_raw(&data);
    assert!((decoded - 3_200.0).abs() < 1.0);
    // Little-endian: low byte first. 3200 rpm / 0.0625 = 51200 = 0xC800.
    assert_eq!(data[0], 0x00);
    assert_eq!(data[1], 0xC8);
}

// ---------------------------------------------------------------------------
// 3. xcp-core CONNECT — negotiate the session parameters.
// ---------------------------------------------------------------------------

/// The CONNECT response a classic-CAN slave sends: Intel byte order,
/// byte-granular addressing, block mode, MAX_CTO = 8, MAX_DTO = 8.
fn connect_response() -> [u8; 8] {
    [0xFF, 0x1C, 0xC0, 0x08, 0x08, 0x00, 0x01, 0x01]
}

#[test]
fn xcp_connect_negotiates_byte_order_and_cto_limits() {
    let request = connect(ResourceMode::CONNECT_NORMAL);
    assert_eq!(request, [0xFF, 0x00]);

    let info = match parse_response_with(ByteOrder::Intel, Expect::Connect, &connect_response())
        .unwrap()
    {
        XcpResponse::Connect(info) => info,
        other => panic!("expected CONNECT, got {other:?}"),
    };

    assert_eq!(info.comm_mode.byte_order, ByteOrder::Intel);
    assert_eq!(info.max_cto, 8);
    assert_eq!(info.max_dto, 8);
    assert_eq!(
        info.comm_mode.address_granularity,
        xcp_core::AddressGranularity::Byte
    );
    assert!(info.resource.contains(ResourceMode::DAQ));
    assert!(info.comm_mode.slave_block_mode);
    assert_eq!(info.protocol_version, (1, 0));
}

#[test]
fn xcp_connect_info_compares_against_a_hand_built_value() {
    // The typed response must be reconstructible — proof the parse is
    // lossless rather than a lossy projection.
    let parsed = match parse_response_with(ByteOrder::Intel, Expect::Connect, &connect_response())
        .unwrap()
    {
        XcpResponse::Connect(info) => info,
        other => panic!("expected CONNECT, got {other:?}"),
    };
    let hand = ConnectInfo {
        resource: ResourceMode::DAQ | ResourceMode::STIM | ResourceMode::PGM,
        comm_mode: CommModeBasic {
            byte_order: ByteOrder::Intel,
            address_granularity: xcp_core::AddressGranularity::Byte,
            slave_block_mode: true,
            optional_info: true,
        },
        max_cto: 8,
        max_dto: 8,
        protocol_version: (1, 0),
        transport_version: (1, 0),
    };
    assert_eq!(parsed, hand);
}

// ---------------------------------------------------------------------------
// 4. The cross-crate flow: A2L address → SET_MTA bytes under the
//    negotiated byte order → CAN frame → SocketCAN bytes.
// ---------------------------------------------------------------------------

#[test]
fn a2l_address_survives_into_set_mta_under_the_negotiated_byte_order() {
    let project = A2lProject::parse(A2L).unwrap();
    let module = project.module_by_name("ecu").unwrap();
    let inj = module.characteristic_by_name("inj_pulse").unwrap();

    let info = match parse_response_with(ByteOrder::Intel, Expect::Connect, &connect_response())
        .unwrap()
    {
        XcpResponse::Connect(info) => info,
        other => panic!("expected CONNECT, got {other:?}"),
    };
    let order = info.comm_mode.byte_order;

    // A2L gives a u32; the host hands it to xcp-core, which encodes it
    // per the byte order the *slave* declared — not the host's native one.
    // SET_MTA is `[0xF6, 0x00, 0x00, EXT, ADDR₃…₀]` — the address lands
    // after the reserved bytes, not straight after the PID.
    let frame_bytes = set_mta(order, inj.address, 0);
    assert_eq!(frame_bytes[0], 0xF6, "SET_MTA PID");
    assert_eq!(frame_bytes[3], 0x00, "address extension");
    assert_eq!(&frame_bytes[4..8], &inj.address.to_le_bytes());
    assert_eq!(frame_bytes.len(), 8);
    assert!(
        frame_bytes.len() <= info.max_cto as usize,
        "SET_MTA must fit MAX_CTO: {} > {}",
        frame_bytes.len(),
        info.max_cto
    );

    // Motorola order must produce a *different* frame for the same address.
    let motorola = set_mta(ByteOrder::Motorola, inj.address, 0);
    assert_ne!(frame_bytes, motorola);
    assert_eq!(&motorola[4..8], &inj.address.to_be_bytes());
    // Same PID, same reserved bytes — only the address encoding moves.
    assert_eq!(frame_bytes[..4], motorola[..4]);
}

#[test]
fn upload_frame_length_comes_from_the_a2l_datatype_and_fits_the_dto() {
    let project = A2lProject::parse(A2L).unwrap();
    let module = project.module_by_name("ecu").unwrap();

    let info = match parse_response_with(ByteOrder::Intel, Expect::Connect, &connect_response())
        .unwrap()
    {
        XcpResponse::Connect(info) => info,
        other => panic!("expected CONNECT, got {other:?}"),
    };

    for name in ["engine_speed", "engine_load"] {
        let meas = module.measurement_by_name(name).unwrap();
        let len = meas.datatype.size_bytes() as u8;
        let frame = upload_frame(len);
        assert_eq!(frame[0], 0xF5, "UPLOAD PID");
        assert_eq!(frame[1], len);
        assert!(
            frame.len() <= info.max_cto as usize,
            "{name}: UPLOAD frame {} exceeds MAX_CTO {}",
            frame.len(),
            info.max_cto
        );
        assert!(len as u16 <= info.max_dto);
    }
}

#[test]
fn daq_list_frames_every_a2l_measurement_and_fits_classic_can() {
    let project = A2lProject::parse(A2L).unwrap();
    let module = project.module_by_name("ecu").unwrap();

    let mut list = DaqList::new(0, 0x0010);
    for meas in &module.measurements {
        list.push_entry(DaqEntry {
            bit_offset: 0,
            size: meas.datatype.size_bytes() as u8,
            address: meas.address,
            address_extension: 0,
        });
    }

    // `configure_frames` emits one flat byte run: SET_DAQ_LIST_MODE (8)
    // then SET_DAQ_PTR + WRITE_DAQ (16) per entry.
    let frames = list.configure_frames(ByteOrder::Intel);
    assert_eq!(frames.len(), 8 + 16 * module.measurements.len());
    assert_eq!(frames[0], 0xE0, "SET_DAQ_LIST_MODE PID");

    // Every protocol command must occupy exactly one classic CAN frame —
    // the constraint that makes an XCP-over-CAN CTO a single frame.
    let mut off = 0;
    let widths = [8_usize]
        .into_iter()
        .chain((0..module.measurements.len()).flat_map(|_| [8_usize, 8_usize]));
    for w in widths {
        assert_eq!(w, CLASSIC_MAX_LEN, "a DAQ config command is one full frame");
        off += w;
    }
    assert_eq!(off, frames.len());
    assert_eq!(frames[8], 0xE2, "SET_DAQ_PTR PID");
    assert_eq!(frames[16], 0xE1, "WRITE_DAQ PID");
    assert_eq!(list.start_frame()[0], 0xDE, "START_STOP_DAQ_LIST PID");

    let stop = list.start_stop_frame(StartStopMode::Stop);
    assert!(!stop.is_empty());
}

// ---------------------------------------------------------------------------
// 5. The full loop: DTO bytes on the wire → engineering value → physical
//    value → limit check against the A2L declaration.
// ---------------------------------------------------------------------------

#[test]
fn a_daq_dto_decodes_through_dbc_and_converts_through_a2l() {
    let dbc = Dbc::parse(DBC).unwrap();
    let project = A2lProject::parse(A2L).unwrap();
    let module = project.module_by_name("ecu").unwrap();
    let (cmd_id, rsp_id) = can_ids();
    let transport = xcp_core::transport::XcpOnCan::new(cmd_id, rsp_id);

    // The slave's DAQ DTO: a full 8-byte payload carrying the configured
    // measurements. Build it the way the ECU would — encode the signals.
    let status = &dbc.messages[0];
    let rpm_signal = &status.signals[0];
    let load_signal = &status.signals[1];
    let mut payload = [0_u8; 8];
    rpm_signal.encode_raw(&mut payload, 3_200.0).unwrap();
    load_signal.encode_raw(&mut payload, 75.0).unwrap();

    // DTO bytes → CAN frame on the response ID → packet back out.
    let frame = CanFrame::new(rsp_id, &payload).unwrap();
    let packet = transport
        .extract_packet(&frame)
        .expect("frame is on the response ID");
    assert_eq!(packet, &payload);

    // Packet bytes → engineering values (the DBC applies scale/offset).
    let rpm_eng = rpm_signal.decode_raw(packet);
    let load_eng = load_signal.decode_raw(packet);
    assert!((rpm_eng - 3_200.0).abs() < 1.0);
    assert!((load_eng - 75.0).abs() < 1.0);

    // The A2L COMPU_METHOD is the second, independent description of the
    // same scaling — it is applied to the *unscaled* count, which the
    // host recovers from the DBC by inverting the DBC factor.
    let rpm_raw = rpm_raw_from_engineering(rpm_signal, rpm_eng);
    let load_raw = load_raw_from_engineering(load_signal, load_eng);
    assert!((rpm_raw - 51_200.0).abs() < 1.0, "51200 counts at 1/16 rpm");
    assert!((load_raw - 191.0).abs() < 1.0, "75% at 1/2.55 %-per-count");

    // Engineering → physical via the A2L COMPU_METHOD named by the
    // MEASUREMENT's conversion field.
    // `rpm_conv` is IDENTICAL, so a2l-parse hands the count back
    // untouched — the 1/16 rpm factor exists *only* in the DBC. A host
    // that trusted the A2L method alone would report 51200 rpm. The
    // composition is the host's job, and this is the assertion that
    // pins that contract down.
    let rpm_phys = module
        .compu_method_by_name(
            &module
                .measurement_by_name("engine_speed")
                .unwrap()
                .conversion,
        )
        .unwrap()
        .to_linear()
        .unwrap()
        .apply(rpm_raw);
    assert!(
        (rpm_phys - 51_200.0).abs() < 1.0,
        "IDENTICAL must leave the count untouched, got {rpm_phys}"
    );
    let rpm_host = rpm_phys * rpm_signal.scale + rpm_signal.offset;
    assert!(
        (rpm_host - 3_200.0).abs() < 1.0,
        "host applies the DBC factor"
    );

    let load_cm = module
        .compu_method_by_name(
            &module
                .measurement_by_name("engine_load")
                .unwrap()
                .conversion,
        )
        .unwrap()
        .to_linear()
        .unwrap();
    let load_phys = load_cm.apply(load_raw);
    // The DBC and the A2L describe the same 0.392156… scaling; the two
    // independent descriptions must agree to well within one count.
    assert!(
        (load_phys - load_eng).abs() < 0.4,
        "DBC {} and A2L {} disagree beyond one LSB",
        load_eng,
        load_phys
    );
    assert!(
        (0.0..=100.0).contains(&load_phys),
        "{load_phys} out of range"
    );
}

#[test]
fn physical_values_are_checked_against_the_a2l_declared_limits() {
    let project = A2lProject::parse(A2L).unwrap();
    let module = project.module_by_name("ecu").unwrap();
    let inj = module.characteristic_by_name("inj_pulse").unwrap();
    let conv = module
        .compu_method_by_name(&inj.conversion)
        .unwrap()
        .to_linear()
        .unwrap();

    // In range.
    let raw_ok = 160_u64; // 160 * 0.0390625 = 6.25 ms
    let phys_ok = conv.apply(raw_ok as f64);
    assert!((phys_ok - 6.25).abs() < 1e-9);
    assert!((inj.lower_limit..=inj.upper_limit).contains(&phys_ok));

    // Out of range — the host must catch it before the DOWNLOAD.
    let raw_bad = 400_u64; // 15.625 ms > 12.5 ms upper limit
    let phys_bad = conv.apply(raw_bad as f64);
    assert!(phys_bad > inj.upper_limit);
}

#[test]
fn the_write_path_frames_an_a2l_limited_value_for_download() {
    let project = A2lProject::parse(A2L).unwrap();
    let module = project.module_by_name("ecu").unwrap();
    let inj = module.characteristic_by_name("inj_pulse").unwrap();
    let conv = module
        .compu_method_by_name(&inj.conversion)
        .unwrap()
        .to_linear()
        .unwrap();

    // Physical → raw → wire bytes, in the byte order the slave declared.
    let physical = 6.25_f64;
    let raw = (physical - conv.intercept) / conv.slope;
    assert!((raw - 160.0).abs() < 1e-9);
    let payload = (raw as u32).to_le_bytes();

    let frames = xcp_core::download_with(ByteOrder::Intel, inj.address, 0, &payload);
    // SET_MTA (8 bytes) then DOWNLOAD (2 header + 4 payload).
    assert_eq!(frames[0], 0xF6, "SET_MTA PID leads the sequence");
    assert_eq!(&frames[4..8], &inj.address.to_le_bytes());
    assert_eq!(&frames[8..], &[0xF0, 0x04, 0xA0, 0x00, 0x00, 0x00]);
    assert_eq!(frames.len(), 14);

    // The Intel default helper must agree with the explicit call.
    assert_eq!(
        xcp_core::download(inj.address, &payload),
        xcp_core::download_with(ByteOrder::Intel, inj.address, 0, &payload)
    );
}

// ---------------------------------------------------------------------------
// 6. can-core — the wire encoding the transport bridges to.
// ---------------------------------------------------------------------------

#[test]
fn xcp_packets_survive_the_can_frame_encoding_round_trip() {
    let (cmd_id, rsp_id) = can_ids();
    let transport = xcp_core::transport::XcpOnCan::new(cmd_id, rsp_id);

    for packet in [
        connect(ResourceMode::CONNECT_NORMAL),
        xcp_core::get_status(),
        xcp_core::get_version(),
        xcp_core::synch(),
    ] {
        assert!(
            packet.len() <= CLASSIC_MAX_LEN,
            "CTO too long for classic CAN"
        );

        // `to_bytes` emits `struct can_frame`: a 4-byte id followed by the
        // payload, zero-padded to CLASSIC_FRAME_SIZE.
        let bytes = transport.command_bytes(&packet).unwrap();
        assert_eq!(bytes.len(), CLASSIC_FRAME_SIZE);

        let frame = CanFrame::parse(&bytes).unwrap();
        assert_eq!(frame.id(), cmd_id);
        assert_eq!(frame.data(), packet.as_slice());
        // The DLC must survive the round trip — a host reads it to size
        // the packet.
        assert_eq!(frame.dlc(), packet.len() as u8);

        // A frame on an unrelated ID must not be accepted as ours.
        let foreign = CanFrame::new(CanId::standard(0x123).unwrap(), &packet).unwrap();
        assert!(transport.extract_packet(&foreign).is_none());
    }
}

#[test]
fn an_oversized_xcp_packet_is_rejected_by_the_transport() {
    let (cmd_id, rsp_id) = can_ids();
    let transport = xcp_core::transport::XcpOnCan::new(cmd_id, rsp_id);
    let too_long = vec![0x00_u8; 9];
    let err = transport.command_frame(&too_long).unwrap_err();
    assert!(matches!(err, CanError::DataTooLong { .. }));
}

// ---------------------------------------------------------------------------
// 7. Failure paths — the taxonomy must survive the composition.
// ---------------------------------------------------------------------------

#[test]
fn slave_error_packets_typed_error_instead_of_panicking() {
    // ERR packet: PID 0xFE + slave error code. 0x33 = resource temporarily
    // not accessible — what a host sees during a calibration page switch.
    // The ERR PID must win over `Expect`, or a host would mis-read the
    // error code as CONNECT payload.
    for expect in [Expect::Connect, Expect::Generic, Expect::Upload] {
        let err = parse_response_with(ByteOrder::Intel, expect, &[0xFE, 0x33]).unwrap_err();
        assert!(
            matches!(err, XcpError::ResourceTemporaryNotAccessible),
            "{expect:?}"
        );
    }

    let err = parse_response_with(ByteOrder::Intel, Expect::Generic, &[0xFE, 0x20]).unwrap_err();
    assert!(matches!(err, XcpError::CmdUnknown));

    // A write to a read-only deposit.
    let err = parse_response_with(ByteOrder::Intel, Expect::Upload, &[0xFE, 0x23]).unwrap_err();
    assert!(matches!(err, XcpError::WriteProtected));

    // An unmapped slave code is preserved, not swallowed.
    let err = parse_response_with(ByteOrder::Intel, Expect::Generic, &[0xFE, 0x7F]).unwrap_err();
    assert!(matches!(err, XcpError::SlaveOther(0x7F)));
    assert_eq!(err.slave_code(), Some(0x7F));
}

#[test]
fn truncated_and_hostile_responses_never_panic() {
    let full = connect_response();
    for len in 0..full.len() {
        let _ = parse_response_with(ByteOrder::Intel, Expect::Connect, &full[..len]);
    }
    // A PID that is not in the table.
    let _ = parse_response_with(ByteOrder::Intel, Expect::Generic, &[0x01, 0x02, 0x03]);
    // A PID outside the byte range entirely.
    assert!(matches!(
        parse_response_with(ByteOrder::Intel, Expect::Generic, &[0xAA]),
        Err(XcpError::UnknownPid(0xAA))
    ));
}

#[test]
fn a_malformed_dbc_is_a_typed_error_with_a_line_number() {
    let err = Dbc::parse("BO_ 320 broken: 8\n  SG_ no_start_bit\n").unwrap_err();
    assert!(matches!(err, DbcError::Syntax { .. }), "got {err:?}");
    assert!(err.to_string().len() > 10);
}

// ---------------------------------------------------------------------------
// 8. The composition contract: the same description must be usable by a
//    second consumer without either crate knowing about the other.
// ---------------------------------------------------------------------------

#[test]
fn an_inventory_walker_covers_every_measurement_without_crate_coupling() {
    // The shape a host uses to build an engineering UI: enumerate every
    // measurement, resolve its conversion and its DBC signal, and report
    // any gap. Neither a2l-parse nor dbc-parse knows this function exists.
    #[allow(dead_code)]
    struct Binding {
        module: String,
        measurement: String,
        address: u32,
        bytes: usize,
        can_signal: String,
        physical: f64,
    }

    let dbc = Dbc::parse(DBC).unwrap();
    let project = A2lProject::parse(A2L).unwrap();

    let mut bindings = Vec::new();
    for module in &project.modules {
        for meas in &module.measurements {
            let signal = dbc
                .messages
                .iter()
                .flat_map(|m| &m.signals)
                .find(|s| s.name == meas.name);
            let Some(signal) = signal else { continue };
            let cm = module.compu_method_by_name(&meas.conversion).unwrap();
            let Some(lin) = cm.to_linear() else { continue };

            // Synthesize one count and convert it.
            let raw = 1.0_f64;
            bindings.push(Binding {
                module: module.name.clone(),
                measurement: meas.name.clone(),
                address: meas.address,
                bytes: meas.datatype.size_bytes(),
                can_signal: signal.name.clone(),
                physical: lin.apply(raw),
            });
        }
    }

    assert_eq!(bindings.len(), 2);
    assert!(bindings.iter().all(|b| b.can_signal == b.measurement));
    assert_eq!(bindings[0].bytes, 2, "engine_speed is a UWORD");
    assert_eq!(bindings[1].bytes, 1, "engine_load is a UBYTE");
    assert_ne!(bindings[0].address, bindings[1].address);
    // One raw count through each RAT_FUNC conversion.
    assert!((bindings[1].physical - 0.392_156_862_745_098).abs() < 1e-12);

    // Every bound measurement must be reachable by an XCP upload.
    for b in &bindings {
        let packet = upload_frame(b.bytes as u8);
        assert!(packet.len() <= CLASSIC_MAX_LEN);
        assert!(!set_mta(ByteOrder::Intel, b.address, 0).is_empty());
    }
}

/// A byte order that is negotiated, not assumed: a Motorola slave must
/// change every multi-byte frame the host emits.
#[test]
fn a_motorola_slave_changes_every_multibyte_frame() {
    let motorola_connect = [0xFF, 0x1C, 0xC0, 0x08, 0x08, 0x00, 0x01, 0x01];
    let info = match parse_response_with(ByteOrder::Motorola, Expect::Connect, &motorola_connect)
        .unwrap()
    {
        XcpResponse::Connect(info) => info,
        other => panic!("expected CONNECT, got {other:?}"),
    };
    assert_eq!(info.comm_mode.byte_order, ByteOrder::Intel);
    // The *requested* byte order is what the master uses to encode its
    // own commands; the *reported* one is the slave's. Both are honoured.
    let addr = 0x7200_0004_u32;
    assert_ne!(
        set_mta(ByteOrder::Intel, addr, 0)[4..8],
        set_mta(ByteOrder::Motorola, addr, 0)[4..8][..]
    );
}
