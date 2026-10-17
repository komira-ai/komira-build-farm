//! The `kbf.log.v1` control commands: round trips over generated values, and every
//! refusal of the decoder and the encoder.
//!
//! Catches:
//! - a conversion that loses or mixes up a field of any control command or farm input
//!   (a round trip over generated values, `decode(encode(x)) == x`);
//! - a decoder that reads an unspecified or unknown enum value as a default, or an
//!   unset `oneof` as nothing at all, instead of refusing it;
//! - a decoder that accepts an absent required field, a zero term or machine version,
//!   a short checkpoint digest, or a value its own type refuses (a custom QoS level, a
//!   capability request);
//! - a decoder that accepts bytes `encode` would not write for the value: a field
//!   repeated, a zero value written, an unknown field, a map or set out of order or
//!   with a repeat, a capability request in another order or with a key `parse` skips;
//! - an encoder that writes a wall-clock time it cannot represent.

mod control_support;

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};
use std::time::{Duration, SystemTime};

use control_support::{batch, dec, enc, wall};
use kbf_caps::{Arch, Consumable, CpuCaps, NodeCaps, RequestError};
use kbf_log::{
    ActionEntry, Checkpoint, DecodeError, EncodeError, FarmBatch, FarmInput, Format, LogCommand,
    MachineVersion, Report, Takeover, encode,
};
use kbf_meta::{ActionRecord, Closure};
use kbf_proto::kbf::log::v1 as proto;
use kbf_sched::{DaemonInstance, Request};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Failure, FarmTime, LeaseGrant, LeaseId,
    LeaseKind, LostWhy, OperationId, Outcome, Qos, QosError, RefusalRecord, Resources,
    ResultRecord, WorkerId,
};
use prost::Message;

/// xorshift64*, so a failing value is reproducible from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Clone>(&mut self, items: &[T]) -> T {
        items[usize::try_from(self.below(items.len() as u64)).expect("index")].clone()
    }

    /// Small values often (the zero values proto3 leaves out included), any u64 else.
    fn number(&mut self) -> u64 {
        match self.below(3) {
            0 => self.below(3),
            1 => self.below(1 << 14),
            _ => self.next(),
        }
    }

    fn time(&mut self) -> FarmTime {
        FarmTime::from_millis(self.number())
    }

    fn digest(&mut self) -> Digest {
        let mut hash = [0u8; 32];
        for chunk in hash.chunks_mut(8) {
            chunk.copy_from_slice(&self.next().to_le_bytes()[..chunk.len()]);
        }
        Digest::new(DigestFunction::Sha256, hash, self.number())
    }

    /// A short name, the empty one included.
    fn name(&mut self) -> String {
        let len = self.below(4);
        (0..len)
            .map(|_| char::from(b'a' + u8::try_from(self.below(26)).expect("letter")))
            .collect()
    }

    fn worker(&mut self) -> WorkerId {
        WorkerId::new(self.name())
    }

    fn lease(&mut self) -> LeaseId {
        LeaseId::new(self.number(), self.number())
    }

    fn resources(&mut self) -> Resources {
        Resources::new(self.number(), self.number())
            .with_gpus(self.number())
            .with_vms(self.number())
    }

    fn names(&mut self) -> BTreeSet<String> {
        (0..self.below(3)).map(|_| self.name()).collect()
    }

    fn caps(&mut self) -> NodeCaps {
        let arch = self.pick(&[Arch::X86_64, Arch::Arm64]);
        let mut features = self.names();
        if self.below(2) == 0 {
            // Enough for a level, so the computed level is not always none.
            features.extend(["fp", "asimd", "cmov", "cx8", "fpu", "fxsr", "mmx"].map(String::from));
            features.extend(["syscall", "sse", "sse2"].map(String::from));
        }
        let mut caps = NodeCaps::new(CpuCaps::new(arch, features));
        for _ in 0..self.below(3) {
            caps.exact.insert(self.name(), self.name());
        }
        for _ in 0..self.below(3) {
            let values = self.names();
            caps.members.insert(self.name(), values);
        }
        let all = [
            Consumable::Cpus,
            Consumable::MemGib,
            Consumable::NvmeGib,
            Consumable::Gpus,
        ];
        for _ in 0..self.below(3) {
            caps.consumables.insert(self.pick(&all), self.number());
        }
        caps.drivers = self.names();
        for _ in 0..self.below(3) {
            let attributes = (0..self.below(3))
                .map(|_| (self.name(), self.name()))
                .collect();
            caps.devices.insert(self.name(), attributes);
        }
        caps
    }

    fn qos(&mut self) -> Qos {
        match self.below(4) {
            0 => Qos::Interactive,
            1 => Qos::Ci,
            2 => Qos::Batch,
            _ => {
                let urgency = self.pick(&[0, 1, 150, 250, u16::MAX]);
                Qos::custom(format!("q{}", self.name()), urgency).expect("custom level")
            }
        }
    }

    fn needs(&mut self) -> kbf_caps::Request {
        let image = format!("img@sha256:{}", "0f".repeat(32));
        let pool = [
            ("arch", "x86_64"),
            ("arch", "arm64"),
            ("isa_level", "x86-64-v2"),
            ("cpu.feature", "avx2"),
            ("cpu.feature", "aes"),
            ("cpus", "4"),
            ("gpu", "1"),
            ("os", "linux"),
            ("label.rack", "r1"),
            ("xcode", "16C5032a"),
            ("vm.image", image.as_str()),
        ];
        let mut props: BTreeMap<&str, &str> = BTreeMap::new();
        let mut features = Vec::new();
        for _ in 0..self.below(5) {
            let (k, v) = self.pick(&pool);
            if k == "cpu.feature" {
                features.push((k, v));
            } else {
                props.insert(k, v);
            }
        }
        // An iOS device, alone or with attributes; `os=linux` goes with none.
        let attributes = [
            ("ios.device.class", "iPhone"),
            ("ios.device.product_type", "iPhone17,3"),
            ("ios.device.os_version", "26.0"),
            ("ios.device.os_build", "23A341"),
        ];
        if self.below(4) == 0 && props.get("os") != Some(&"linux") {
            props.insert("ios.device", "1");
            for _ in 0..self.below(5) {
                let (k, v) = self.pick(&attributes);
                props.insert(k, v);
            }
        }
        let all = props.into_iter().chain(features);
        kbf_caps::Request::parse(all).expect("request")
    }

    fn request(&mut self) -> Request {
        Request {
            key: ActionKey {
                instance: self.name(),
                action: self.digest(),
            },
            qos: self.qos(),
            kind: self.pick(&LeaseKind::ALL),
            resources: self.resources(),
            needs: self.needs(),
            hermetic: self.below(2) == 0,
            do_not_cache: self.below(2) == 0,
        }
    }

    fn outcome(&mut self) -> Outcome {
        let failures = [
            Failure::Timeout,
            Failure::Infra,
            Failure::Invalid,
            Failure::OutOfMemory,
            Failure::NodeMemoryPressure,
            Failure::MissingInput,
            Failure::Lost(LostWhy::DaemonRestarted),
            Failure::Lost(LostWhy::Fenced),
        ];
        if self.below(2) == 0 {
            Outcome::Completed {
                action_result: self.digest(),
            }
        } else {
            Outcome::Failed(self.pick(&failures))
        }
    }

    fn record(&mut self) -> ControlRecord {
        match self.below(3) {
            0 => ControlRecord::Lease(LeaseGrant {
                lease: self.lease(),
                operation: OperationId(self.number()),
                worker: self.worker(),
            }),
            1 => ControlRecord::Result(ResultRecord {
                lease: self.lease(),
                operation: OperationId(self.number()),
                outcome: self.outcome(),
            }),
            _ => ControlRecord::Refusal(RefusalRecord {
                operation: OperationId(self.number()),
                reason: self.name(),
            }),
        }
    }

    fn input(&mut self) -> FarmInput {
        match self.below(11) {
            0 => FarmInput::WorkerUp {
                worker: self.worker(),
                instance: DaemonInstance::new(self.name()),
                predecessor_stopped: self.below(2) == 0,
                capacity: self.resources(),
                caps: self.caps(),
            },
            1 => FarmInput::Capacity {
                worker: self.worker(),
                capacity: self.resources(),
                caps: self.caps(),
            },
            2 => FarmInput::Heartbeat {
                worker: self.worker(),
                running: (0..self.below(4)).map(|_| self.lease()).collect(),
            },
            3 => FarmInput::Submit(self.request()),
            4 => FarmInput::Committed(self.record()),
            5 => FarmInput::Started {
                operation: OperationId(self.number()),
                lease: self.lease(),
            },
            6 => FarmInput::Tick,
            7 => FarmInput::Cordon(self.worker()),
            8 => FarmInput::Drain {
                worker: self.worker(),
                deadline: self.time(),
            },
            9 => FarmInput::Uncordon(self.worker()),
            _ => FarmInput::Disconnected(self.worker()),
        }
    }

    fn command(&mut self) -> LogCommand {
        match self.below(5) {
            0 => LogCommand::Farm(FarmBatch {
                now: self.time(),
                wall: wall(self.number()),
                inputs: (0..self.below(4)).map(|_| self.input()).collect(),
            }),
            1 => LogCommand::Report(Report {
                now: self.time(),
                operation: OperationId(self.number()),
                lease: self.lease(),
                outcome: self.outcome(),
                action_entry: (self.below(2) == 0).then(|| ActionEntry {
                    action: self.digest(),
                    record: ActionRecord {
                        result: self.digest(),
                        closure: (0..self.below(3))
                            .map(|_| self.digest())
                            .collect::<Closure>(),
                    },
                }),
            }),
            2 => LogCommand::Takeover(Takeover {
                term: NonZeroU64::new(self.number().max(1)).expect("non-zero"),
                now: self.time(),
            }),
            3 => {
                let mut digest = [0u8; 32];
                digest[..8].copy_from_slice(&self.next().to_le_bytes());
                LogCommand::Checkpoint(Checkpoint {
                    index: self.number(),
                    digest,
                })
            }
            _ => LogCommand::MachineVersion(MachineVersion::new(
                NonZeroU32::new(
                    u32::try_from(self.number() % u64::from(u32::MAX)).expect("u32") + 1,
                )
                .expect("non-zero"),
            )),
        }
    }
}

/// Catches a conversion that loses or mixes up a field: 20 000 generated control
/// commands (every command, input, record, outcome, enum value and QoS kind among
/// them) decode to themselves and re-encode to the same bytes.
#[test]
fn generated_control_commands_round_trip() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut commands = [0u32; 5];
    let mut inputs = [0u32; 11];
    for n in 0..20_000 {
        let command = rng.command();
        let bytes = enc(&command);
        let back = dec(&bytes).unwrap_or_else(|e| panic!("value {n}: {command:?}: {e}"));
        assert_eq!(back, command, "value {n}");
        assert_eq!(enc(&back), bytes, "value {n}: re-encoded differently");
        commands[kind(&command)] += 1;
        if let LogCommand::Farm(b) = &command {
            for input in &b.inputs {
                inputs[input_kind(input)] += 1;
            }
        }
    }
    // Not vacuous: every command and every input was generated, many times over.
    assert!(commands.iter().all(|&n| n > 1_000), "{commands:?}");
    assert!(inputs.iter().all(|&n| n > 100), "{inputs:?}");
}

const fn kind(command: &LogCommand) -> usize {
    match command {
        LogCommand::Meta(_) | LogCommand::Bootstrap(_) | LogCommand::Farm(_) => 0,
        LogCommand::Report(_) => 1,
        LogCommand::Takeover(_) => 2,
        LogCommand::Checkpoint(_) => 3,
        LogCommand::MachineVersion(_) => 4,
    }
}

const fn input_kind(input: &FarmInput) -> usize {
    match input {
        FarmInput::WorkerUp { .. } => 0,
        FarmInput::Capacity { .. } => 1,
        FarmInput::Heartbeat { .. } => 2,
        FarmInput::Submit(_) => 3,
        FarmInput::Committed(_) => 4,
        FarmInput::Started { .. } => 5,
        FarmInput::Tick => 6,
        FarmInput::Cordon(_) => 7,
        FarmInput::Drain { .. } => 8,
        FarmInput::Uncordon(_) => 9,
        FarmInput::Disconnected(_) => 10,
    }
}

// ---- Refusals. Each builds the message with prost, so the bytes are well formed and
// only the one thing under test is wrong. ----

fn wrap(command: proto::log_command::Command) -> Vec<u8> {
    proto::LogCommand {
        command: Some(command),
    }
    .encode_to_vec()
}

fn wrap_input(input: proto::farm_input::Input) -> Vec<u8> {
    wrap(proto::log_command::Command::Farm(proto::FarmBatch {
        farm_time_ms: 1,
        wall_time_ns: 2,
        inputs: vec![proto::FarmInput { input: Some(input) }],
    }))
}

fn pd(n: u8) -> proto::Digest {
    proto::Digest {
        function: 1,
        hash: vec![n; 32],
        size_bytes: u64::from(n),
    }
}

fn pcaps(arch: i32, consumable: i32) -> proto::NodeCaps {
    proto::NodeCaps {
        cpu: Some(proto::CpuCaps {
            arch,
            features: vec![],
        }),
        consumables: vec![proto::ConsumableAmount {
            consumable,
            amount: 1,
        }],
        ..Default::default()
    }
}

fn capacity(caps: Option<proto::NodeCaps>) -> Vec<u8> {
    wrap_input(proto::farm_input::Input::Capacity(proto::Capacity {
        worker: "w".to_owned(),
        capacity: Some(proto::Resources::default()),
        caps,
    }))
}

fn prequest(qos: Option<proto::Qos>, kind: i32, needs: &[(&str, &str)]) -> proto::Request {
    proto::Request {
        key: Some(proto::ActionKey {
            instance: String::new(),
            action: Some(pd(1)),
        }),
        qos,
        kind,
        resources: Some(proto::Resources::default()),
        needs: needs
            .iter()
            .map(|(k, v)| proto::Property {
                key: (*k).to_owned(),
                value: (*v).to_owned(),
            })
            .collect(),
        hermetic: false,
        do_not_cache: false,
    }
}

fn submit(request: proto::Request) -> Vec<u8> {
    wrap_input(proto::farm_input::Input::Submit(proto::Submit {
        request: Some(request),
    }))
}

fn builtin(n: i32) -> Option<proto::Qos> {
    Some(proto::Qos {
        level: Some(proto::qos::Level::Builtin(n)),
    })
}

fn custom(name: &str, urgency: u32) -> Option<proto::Qos> {
    Some(proto::Qos {
        level: Some(proto::qos::Level::Custom(proto::CustomQos {
            name: name.to_owned(),
            urgency,
        })),
    })
}

fn report(outcome: Option<proto::Outcome>) -> Vec<u8> {
    wrap(proto::log_command::Command::Report(proto::Report {
        farm_time_ms: 0,
        operation: 0,
        lease: Some(proto::LeaseId::default()),
        outcome,
        action_entry: None,
    }))
}

fn failed(n: i32) -> Option<proto::Outcome> {
    Some(proto::Outcome {
        outcome: Some(proto::outcome::Outcome::Failed(n)),
    })
}

fn unknown(field: &'static str, value: i32) -> Result<LogCommand, DecodeError> {
    Err(DecodeError::UnknownValue { field, value })
}

/// Catches a decoder that defaults an enum value it does not know, or the unspecified
/// zero (which is also what an absent field reads as), instead of refusing it: every
/// enum the control commands carry, at 0 and at a value `log.proto` does not name.
#[test]
fn an_unspecified_or_unknown_enum_value_is_refused() {
    for value in [0, 99] {
        assert_eq!(
            dec(&capacity(Some(pcaps(value, 1)))),
            unknown("CpuCaps.arch", value)
        );
        assert_eq!(
            dec(&capacity(Some(pcaps(1, value)))),
            unknown("ConsumableAmount.consumable", value)
        );
        assert_eq!(
            dec(&submit(prequest(builtin(1), value, &[]))),
            unknown("Request.kind", value)
        );
        assert_eq!(
            dec(&submit(prequest(builtin(value), 1, &[]))),
            unknown("Qos.builtin", value)
        );
        assert_eq!(
            dec(&report(failed(value))),
            unknown("Outcome.failed", value)
        );
    }
}

/// Catches a decoder that reads an unset `oneof` (a command, input, record, outcome or
/// QoS kind this binary does not know) as some default.
#[test]
fn an_unset_oneof_is_refused() {
    let none = |name| Err(DecodeError::UnknownCommand(name));
    let empty_input = wrap(proto::log_command::Command::Farm(proto::FarmBatch {
        inputs: vec![proto::FarmInput { input: None }],
        ..Default::default()
    }));
    assert_eq!(dec(&empty_input), none("FarmInput"));
    let record = proto::ControlRecord { record: None };
    assert_eq!(
        dec(&wrap_input(proto::farm_input::Input::Committed(record))),
        none("ControlRecord")
    );
    assert_eq!(
        dec(&report(Some(proto::Outcome { outcome: None }))),
        none("Outcome")
    );
    assert_eq!(
        dec(&submit(prequest(Some(proto::Qos { level: None }), 1, &[]))),
        none("Qos")
    );
}

/// Catches a decoder that fills in an absent required field, a zero term or machine
/// version (proto3's absent value), or a checkpoint digest that is not 32 bytes.
#[test]
fn absent_and_out_of_range_values_are_refused() {
    use proto::farm_input::Input as I;
    let missing = |field| Err(DecodeError::Missing(field));
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "Submit.request",
            wrap_input(I::Submit(proto::Submit { request: None })),
        ),
        (
            "Started.lease",
            wrap_input(I::Started(proto::Started::default())),
        ),
        (
            "WorkerUp.capacity",
            wrap_input(I::WorkerUp(proto::WorkerUp::default())),
        ),
        (
            "WorkerUp.caps",
            wrap_input(I::WorkerUp(proto::WorkerUp {
                capacity: Some(proto::Resources::default()),
                ..Default::default()
            })),
        ),
        ("Capacity.caps", capacity(None)),
        ("NodeCaps.cpu", capacity(Some(proto::NodeCaps::default()))),
        (
            "Request.key",
            submit(proto::Request {
                key: None,
                ..prequest(builtin(1), 1, &[])
            }),
        ),
        ("Request.qos", submit(prequest(None, 1, &[]))),
        (
            "Request.resources",
            submit(proto::Request {
                resources: None,
                ..prequest(builtin(1), 1, &[])
            }),
        ),
        (
            "ActionKey.action",
            submit(proto::Request {
                key: Some(proto::ActionKey::default()),
                ..prequest(builtin(1), 1, &[])
            }),
        ),
        (
            "LeaseGrant.lease",
            wrap_input(I::Committed(proto::ControlRecord {
                record: Some(proto::control_record::Record::Lease(
                    proto::LeaseGrant::default(),
                )),
            })),
        ),
        (
            "ResultRecord.lease",
            wrap_input(I::Committed(proto::ControlRecord {
                record: Some(proto::control_record::Record::Result(
                    proto::ResultRecord::default(),
                )),
            })),
        ),
        (
            "ResultRecord.outcome",
            wrap_input(I::Committed(proto::ControlRecord {
                record: Some(proto::control_record::Record::Result(proto::ResultRecord {
                    lease: Some(proto::LeaseId::default()),
                    ..Default::default()
                })),
            })),
        ),
        (
            "Report.lease",
            wrap(proto::log_command::Command::Report(proto::Report::default())),
        ),
        ("Report.outcome", report(None)),
        (
            "ActionEntry.action",
            wrap(proto::log_command::Command::Report(proto::Report {
                lease: Some(proto::LeaseId::default()),
                outcome: failed(1),
                action_entry: Some(proto::ActionEntry::default()),
                ..Default::default()
            })),
        ),
        (
            "ActionEntry.record",
            wrap(proto::log_command::Command::Report(proto::Report {
                lease: Some(proto::LeaseId::default()),
                outcome: failed(1),
                action_entry: Some(proto::ActionEntry {
                    action: Some(pd(1)),
                    record: None,
                }),
                ..Default::default()
            })),
        ),
        (
            "Takeover.term",
            wrap(proto::log_command::Command::Takeover(proto::Takeover {
                term: 0,
                farm_time_ms: 5,
            })),
        ),
        (
            "MachineVersion.version",
            wrap(proto::log_command::Command::MachineVersion(
                proto::MachineVersion { version: 0 },
            )),
        ),
    ];
    for (field, bytes) in cases {
        assert_eq!(dec(&bytes), missing(field), "{field}");
    }
    let short = wrap(proto::log_command::Command::Checkpoint(proto::Checkpoint {
        index: 1,
        digest: vec![0xab; 31],
    }));
    assert_eq!(
        dec(&short),
        Err(DecodeError::HashLength {
            field: "Checkpoint.digest",
            len: 31
        })
    );
    let completed_short = report(Some(proto::Outcome {
        outcome: Some(proto::outcome::Outcome::Completed(proto::Digest {
            hash: vec![1; 3],
            ..pd(1)
        })),
    }));
    assert_eq!(
        dec(&completed_short),
        Err(DecodeError::HashLength {
            field: "Outcome.completed",
            len: 3
        })
    );
}

/// Catches a decoder that builds a value its own type would refuse: a custom QoS level
/// `Qos::custom` refuses or whose urgency does not fit in 16 bits, and a capability
/// request `Request::parse` refuses.
#[test]
fn values_their_types_refuse_are_refused() {
    assert_eq!(
        dec(&submit(prequest(custom("q", 70_000), 1, &[]))),
        Err(DecodeError::Urgency(70_000))
    );
    assert_eq!(
        dec(&submit(prequest(custom("ci", 150), 1, &[]))),
        Err(DecodeError::Qos(QosError::BuiltinName("ci".to_owned())))
    );
    assert_eq!(
        dec(&submit(prequest(custom("q", 200), 1, &[]))),
        Err(DecodeError::Qos(QosError::BuiltinUrgency(200)))
    );
    assert_eq!(
        dec(&submit(prequest(builtin(1), 1, &[("nope", "x")]))),
        Err(DecodeError::Request(RequestError::UnknownKey(
            "nope".to_owned()
        )))
    );
}

/// Catches a lenient decoder: bytes that decode to a value but are not the bytes
/// `encode` writes for it are refused, so one value has one encoding.
#[test]
fn bytes_encode_would_not_write_are_refused() {
    let not_canonical = Err(DecodeError::NotCanonical);
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    // A field repeated: a cordon naming its worker twice (prost keeps the last).
    let mut once = enc(&batch(FarmInput::Cordon(WorkerId::new("w"))));
    assert_eq!(hex::encode(&once), "1a0b080110021a0542030a0177");
    let twice = hex::decode("1a0e080110021a0842060a01770a0177").expect("hex");
    cases.push(("a repeated field", twice));
    // A zero value written: a drain's deadline 0 as `10 00`.
    cases.push((
        "an explicit zero",
        hex::decode("1a0d080110021a074a050a01771000").expect("hex"),
    ));
    // An unknown field inside the tick (field 1 = 1).
    cases.push((
        "an unknown field",
        hex::decode("1a0a080110021a043a020801").expect("hex"),
    ));
    // A non-minimal varint: the farm time 1 as `81 00`.
    once[2..4].copy_from_slice(&[0x08, 0x81]);
    once.insert(4, 0x00);
    once[1] += 1;
    cases.push(("a non-minimal varint", once));
    // Maps and sets out of order or with a repeat.
    let exact = |keys: &[&str]| proto::NodeCaps {
        exact: keys
            .iter()
            .map(|k| proto::Property {
                key: (*k).to_owned(),
                value: String::new(),
            })
            .collect(),
        ..pcaps(1, 1)
    };
    cases.push(("exact out of order", capacity(Some(exact(&["b", "a"])))));
    cases.push(("exact repeated", capacity(Some(exact(&["a", "a"])))));
    let drivers = proto::NodeCaps {
        drivers: vec!["b".to_owned(), "a".to_owned()],
        ..pcaps(1, 1)
    };
    cases.push(("drivers out of order", capacity(Some(drivers))));
    let features = proto::NodeCaps {
        cpu: Some(proto::CpuCaps {
            arch: 1,
            features: vec!["x".to_owned(), "x".to_owned()],
        }),
        ..pcaps(1, 1)
    };
    cases.push(("features repeated", capacity(Some(features))));
    let members = proto::NodeCaps {
        members: vec![proto::MemberSet {
            key: "m".to_owned(),
            values: vec!["b".to_owned(), "a".to_owned()],
        }],
        ..pcaps(1, 1)
    };
    cases.push(("member values out of order", capacity(Some(members))));
    let consumables = proto::NodeCaps {
        consumables: vec![
            proto::ConsumableAmount {
                consumable: 2,
                amount: 1,
            },
            proto::ConsumableAmount {
                consumable: 1,
                amount: 1,
            },
        ],
        ..pcaps(1, 1)
    };
    cases.push(("consumables out of order", capacity(Some(consumables))));
    let device = |id: &str, keys: &[&str]| proto::Device {
        id: id.to_owned(),
        attributes: keys
            .iter()
            .map(|k| proto::Property {
                key: (*k).to_owned(),
                value: String::new(),
            })
            .collect(),
    };
    let devices = proto::NodeCaps {
        devices: vec![device("b", &[]), device("a", &[])],
        ..pcaps(1, 1)
    };
    cases.push(("devices out of order", capacity(Some(devices))));
    let devices = proto::NodeCaps {
        devices: vec![device("a", &[]), device("a", &[])],
        ..pcaps(1, 1)
    };
    cases.push(("devices repeated", capacity(Some(devices))));
    let attributes = proto::NodeCaps {
        devices: vec![device("a", &["os_version", "class"])],
        ..pcaps(1, 1)
    };
    cases.push(("device attributes out of order", capacity(Some(attributes))));
    // A capability request in another order than kbf-caps writes, or with a key
    // `parse` skips.
    cases.push((
        "needs out of order",
        submit(prequest(
            builtin(1),
            1,
            &[("os", "linux"), ("arch", "arm64")],
        )),
    ));
    cases.push((
        "device needs out of order",
        submit(prequest(
            builtin(1),
            1,
            &[("ios.device.class", "iPhone"), ("ios.device", "1")],
        )),
    ));
    cases.push((
        "needs with a reserved key",
        submit(prequest(builtin(1), 1, &[("kbf-lease", "action")])),
    ));
    for (what, bytes) in cases {
        assert_eq!(dec(&bytes), not_canonical, "{what}");
    }
}

/// Catches an encoder that writes a wall-clock time as some other value: one before
/// the Unix epoch, and one past `u64::MAX` nanoseconds after it, are refused; the last
/// representable one round-trips.
#[test]
fn a_wall_time_outside_u64_nanoseconds_is_not_written() {
    let at = |wall| {
        LogCommand::Farm(FarmBatch {
            now: FarmTime::from_millis(0),
            wall,
            inputs: vec![],
        })
    };
    let before = SystemTime::UNIX_EPOCH - Duration::from_nanos(1);
    assert_eq!(encode(Format::V1, &at(before)), Err(EncodeError::WallTime));
    let last = wall(u64::MAX);
    let past = last + Duration::from_nanos(1);
    assert_eq!(encode(Format::V1, &at(past)), Err(EncodeError::WallTime));
    assert_eq!(dec(&enc(&at(last))), Ok(at(last)));
}

/// Catches a control command written or read under a format this build does not know
/// (format 5; formats 1 to 4 all carry control commands, since formats 2 to 4 only add
/// metadata commands).
#[test]
fn control_commands_refuse_an_unknown_format() {
    let command = batch(FarmInput::Tick);
    let format = Format::new(5);
    assert_eq!(encode(format, &command), Err(EncodeError::Format(format)));
    assert_eq!(
        kbf_log::decode(format, &enc(&command)),
        Err(DecodeError::Format(format))
    );
}

/// Catches: a machine version that names the wrong entry format (mutant "every version
/// is format 1": a raised log would still refuse the write tier's commands or
/// `EvictObjects`, or a version-1 log would accept them), and a version this binary
/// does not know given a format instead of none (it must stop the machine, not be read
/// as a known one).
#[test]
fn machine_versions_name_their_entry_formats() {
    assert_eq!(MachineVersion::V1.get(), 1);
    assert_eq!(MachineVersion::V2.get(), 2);
    assert_eq!(MachineVersion::V1.format(), Some(Format::V1));
    assert_eq!(MachineVersion::V2.format(), Some(Format::V2));
    assert_eq!(MachineVersion::V3.get(), 3);
    assert_eq!(MachineVersion::V3.format(), Some(Format::V3));
    assert_eq!(MachineVersion::V4.get(), 4);
    assert_eq!(MachineVersion::V4.format(), Some(Format::V4));
    let v5 = MachineVersion::new(NonZeroU32::new(5).expect("non-zero"));
    assert_eq!(v5.format(), None);
    assert!(
        MachineVersion::V1 < MachineVersion::V2
            && MachineVersion::V2 < MachineVersion::V3
            && MachineVersion::V3 < MachineVersion::V4
            && MachineVersion::V4 < v5
    );
}
